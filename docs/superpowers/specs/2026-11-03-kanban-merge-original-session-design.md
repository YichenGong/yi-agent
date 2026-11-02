# 看板合并回到原开发会话

日期：2026-11-03
状态：方案 B 已选定；已完成设计复核，实施计划已编写；尚未实现
分支：`fix/kanban-merge-original-session`

## 1. 问题与依据

用户希望：确认一张实现卡的 merge 后，看板把合并指令投给原开发 session，由它继续合并；不得静默转交新 session。

当前 `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs::promote_merge_queue` 无条件返回 `DriverAction::Launch`，成功后覆写卡片 `thread_id`。即使原会话存活且空闲，也会另起会话。测试 `the_queue_head_starts_merging_when_the_gate_is_free` 固定了这一行为。

这偏离了 `2026-10-31-kanban-merge-queue-design.md` §4.4。本设计进一步收紧：实现卡的会话未加载时恢复原 thread，不降级 Launch。独立合并卡没有原开发会话，仍可 Launch。

宿主 `server.rs::ServeLauncher` 的 Append 只接受内存 thread。`rpc/thread.rs::thread/resume` 包含真实恢复逻辑，但热恢复会中断 turn；`thread_store.rs::load` 会跳过坏行、吞读错误。不能直接把这两个入口拼起来冒充安全恢复。

## 2. 范围与非目标

覆盖：实现卡合并调度、通用恢复并追加动作、能力与回报契约、最小在飞安全记录、提示词与 skill、相关测试与进度文档。

保持：用户确认后入队、项目合并互斥、全机并发限制、模型判断 Git 结果、独立 `add-merge` 卡、桌面展示机制。

不增加 CLI 动词，不引入 Merging 状态，不删除开发 worktree，不 push、不自动删除源分支，不重做跨进程 exactly-once 系统。仅给已有 `merge-finish` 增加离线人工恢复选项；正常模型流程不使用此选项。

## 3. 不变量

1. 实现卡的 `thread_id` 从 merge-request 到完成不变；`origin_thread_id` 仅为需求讨论来源。
2. 实现卡 `workdir` 保留开发树；合并目标树另行传入，不覆盖开发绑定。
3. 获得全机余量、一份卡片租约和项目合并锁后才能交出动作；已有租约复用，不能重复领取。
4. 忙会话不被取消、重建、interject 或抢占。
5. 缺失、损坏、错目录、权限或恢复失败明确阻塞，不新建替代 thread，不用空历史假装恢复。
6. 只有模型根据 Git 与验证结果声明结果；空闲不等于成功。
7. 指令已投递或是否投递不确定时，不能仅凭卡片非 Running、daemon PID 死亡、flock 可取得就放行下一次合并。
8. 本文中的全机运行数沿用现有宿主进程内观测口径及全局租约限制，不新增跨宿主实时运行数汇总，也不声称比现有机制更强。

## 4. 方案取舍

- A：热会话 Append，冷会话 Launch。冷启动切断上下文，不采用。
- B：热追加、冷恢复同 ID 后追加、不可恢复则报告。用户已选定。
- C：开发 turn 长期不结束等待授权。耦合 turn 与排队生命周期，不采用。

## 5. 插件与宿主分工及契约

插件决定卡片、授权、目标、锁生命周期；宿主只认识通用会话动作，不解释 card/kanban。

### 5.1 动作

保留 launch、append 语义。增加显式动作：

```json
{
  "action_id": "act-<nonce>-<sequence>",
  "kind": "resume_append",
  "thread_id": "thread-original",
  "workdir": "/project/.worktrees/kanban/card",
  "objective": "已获授权的合并指令",
  "env": {"SUPERPOWERS_KANBAN_CARD": "card-id"}
}
```

workdir 是原会话的精确定位目录，不是 merge 目标。仅在该目录的 ThreadStore 查找，不 fallback 到 cfg.cwd、索引中的别处或新 UUID。校验 ID、目录存在性、canonical cwd 一致性；对热 thread 同样核对其 cwd。原 Append 对未知 thread 仍失败。

热 thread 不重建工具、不改运行时 env；env 只用于冷装配。因此合并 objective 中的状态命令必须使用明确 card ID 与绝对 state-dir，不依赖热会话恰好有 `--self` 环境。冷装配必须在实际 bash 工具构造时应用 env；若工具装配失败，不得降级为遗漏 env 的 BuiltAgent 后宣称成功。

### 5.2 能力、结果与兼容

宿主每次 driver/next 增加 `capabilities.actions: ["launch", "append", "resume_append"]`。缺失按不支持处理。实现卡未获能力时保持 MergeQueued，detail 明确升级原因；不发未知动作，不退化 Launch。能力检查与原因展示不依赖本拍 budget 是否为零，相同 detail 不反复落盘或刷日志。独立合并卡首次 Launch 不要求新能力。

结果保留 action_id、ok、thread_id、error，增加可选 `error_code`：busy、thread_not_found、workdir_missing、restore_failed、delivery_failed。旧动作不要求此字段。错误码表示本次 objective **未进入 prompt 通道**；一旦 send 成功，不允许再回报为可安全重试的失败。未知失败、错 ID 成功回报、回报遗失均按不确定处理，不能解析 error 文本猜 busy。

resume_append 成功必须回原 ID。宿主登记该 ID 到驱动观测表并去重。插件拒绝把其他 ID 写回卡片。两端共读的 driver-protocol.json 锁住新字段、旧字段默认值与显式 no-action 行为。

## 6. 调度与状态结算

1. merge-request 只把 AwaitingMerge 改成 MergeQueued 并记时间；重复请求不改顺位。
2. FIFO 扫描：已观测 alive 且非 idle 的实现卡跳过，不占锁、不改时间，可检查后续候选；缺观测允许尝试 resume_append，最终由宿主判断 busy。
3. 缺 thread_id/workdir 转 NeedsYou；不可用能力保留排队。不得猜 origin_thread_id。
4. 先检查全机余量，再取得或复用卡片的一份租约，再尝试非阻塞 merge.lock。顺序一致；没额度不持 merge.lock。已有 idle thread 的租约仍遵守既有租约机制，不另占第二份。
5. 准备 base 所在目标树，检查持久在飞记录为空。所有 git 命令使用 argv 或安全引用；实现卡 source_ref 为空时用 `worktree::slugify` 推导 `kanban/<slug>`。
6. 持久化锁内 CAS 重查 MergeQueued、thread、workdir、原入队时间仍匹配。在飞记录落盘在前，Running 状态落盘在后；两者均完成才返回动作。步骤间失败或崩溃，留下记录阻塞而非重复派发。
7. 内存 PendingKind 区分 ResumeForMerge 与 LaunchForMerge，保存原顺位、期望绑定及本次是否新领租约。在飞动作必须参与 lease 对账，即使 thread 尚未出现也不能回收后再超发。
8. 独立合并卡保持现有 Launch 路径。本次不扩展它的同 thread 重试语义；但两种合并都遵守同一在飞安全记录与释放条件。
9. 成功回报只结算动作；不覆盖模型已经写的 done/needs_you，不改实现卡 thread_id/workdir。独立 Launch 成功仍写其新 thread ID。
10. busy 已证明未投递：仅在卡仍是本次 Running 且绑定未变时退回 MergeQueued，保留原时间。卡已被用户改状态则不覆盖。其他确定未投递错误同样 CAS 转 NeedsYou。重复、过期回报不能结算后续 attempt。

## 7. 严格冷恢复

从现有恢复代码抽取必要的纯装配逻辑，避免整文件重构；不调用会中断热 thread 的 RPC 分支，不复用新建 thread 的 UUID/meta 创建路径。

顺序：校验路径/ID → 取得原 thread 跨进程 writer lock → 严格读取一次权威存储 → 处理 partial → 用持久化配置构造 Session/BuiltAgent → 装配工具/env → 注册 runtime/thread/watcher/driver → 同一 prepare-and-deliver 内核投递。

- 锁 Busy 返回 busy，锁 I/O 返回 restore_failed，不无锁继续。现有 `acquire_thread_lock` 对非 Busy 的 flock 错误仍打印后放行，必须改为返回 ThreadLockError::Io，不能只在上层匹配错误。原 RPC 是否降级仍由其调用者决定。锁句柄覆盖读取、partial 升格、装配、注册，最终转交 ThreadSession 持有。
- 严格读取必须返回真实 I/O/JSON 错误，不 validate 后再调用宽松 load。复用日志/partial 合流和去重的纯函数，严格与宽松入口共享语义，避免复制漂移。
- metadata 必须存在且可解析、ID 与 cwd 匹配；合法存在的空 jsonl 可恢复；只有 meta、没有 log 也没有有效 partial 则拒绝。这包括用户 `/clear` 后形成的 meta-only 冷会话：明确要求用户人工处理，不自动用失忆上下文合并。
- 文件存在但损坏/不可读必须失败。有非空items/工具结果却无可恢复messages的旧记录不能按空上下文续跑，明确拒绝；合法空log不受影响。仅有含可恢复上下文的有效 partial 时可恢复；未收尾 partial 必须在新 turn 前可靠升格。升格/清除失败则不投递新轮，重试按首 item ID 幂等，不覆盖唯一检查点。
- 保留 messages/工具结果、模型覆盖、permission_mode、reasoning_effort、preserve_thinking、extra_roots、anchor_sha、标题、driver_project/ref、usage/totals 和 last input tokens；session Arc 与委派工具必须指向同一恢复会话。
- 沿用原权限和 sandbox 规则，不把 prompt 授权当成文件系统权限提升。目标 base 树不在允许根时按既有权限流程处理；无法授权则 NeedsYou，不能偷偷加根或切 yolo。
- 冷恢复失败只清理本次注册/临时装配，不删原历史/meta、不破坏其他 thread。恢复已完成但投递失败，可以保留可正常手工打开的 idle thread。

首轮走既有 thread-keyed 通知顺序；objective 与结果在原时间线出现，不新增 meta、不发伪 RPC 响应、不重复广播全量历史。prepare/send 失败只回滚本次拥有的 turn 占位；不能清掉已有 busy turn。若已发 turn/started 但 send 失败，补对应错误/结束通知，避免界面永久运行中。

## 8. 合并在飞记录与锁回收

### 8.1 最小持久安全记录

新增 `<state-dir>/merge.attempt.json`，项目任意时刻最多一条，独立于卡片（归档、purge 不会擦掉安全证据）。字段：version=1、action_id、card_id、thread_id（独立 Launch 初始可空）、expected_workdir（派发时卡片目录快照）、phase（pending/accepted/uncertain）、原 merge_queued_at。该记录是安全门，不是待自动重放消息。

所有读写在既有 board.lock 内进行；save 用同目录临时文件、sync_all、rename。缺文件代表无记录；坏 JSON、未知 version、I/O 错误必须阻塞且 list 显示原因，不能按无记录处理。调度必须严格读取 board.json；写看板时，已存在但损坏/不可读文件不能被空看板覆盖，只有 NotFound 可按新看板处理。

锁顺序：daemon 先 Inner mutex，再非阻塞 merge flock，再 board.lock；离线恢复先 plugin.lock，再非阻塞 merge flock，再 board.lock。不存在持 board.lock 等待 Inner 或阻塞 merge flock 的路径。资源准备失败而还没写记录可直接回收；记录已写但未交出动作的失败可在同一进程确认未投递后按 action_id CAS 清理。无法确认则保留 uncertain。

### 8.2 正常释放

accepted 的记录只在以下条件全部满足时自动清除：

- 本进程已收到匹配 action_id 的成功结果，无在飞 pending；
- 卡明确离开 Running（模型/用户声明结果），且绑定未被替换；
- 本次结果之后的一拍里，匹配 thread 明确 alive=true、idle=true。

这里的 idle 沿用宿主 Idle 且无 active_turn 的口径，不是对任意外部后台进程的存活证明。合并与验证必须同步执行、等相关写入进程结束才声明结果；擅自脱离工具管理的进程不在自动证明范围，异常恢复须人工核对。

先持锁清记录，成功后清 holder 并 drop 本进程 gate；磁盘清除失败继续持 gate。模型早写 done 或早执行 merge-finish，都不能绕过 pending/idle 检查。卡消失、绑定改变、观测缺失或 dead 不证明 git 子进程停止，转 uncertain 阻塞。lease 与 gate 分开核算，确定未投递失败只归还本次新领 lease；同时按本次card/action归属清除自己的holder，不能只drop gate留下误导后续调度的残留，也不能删他人holder。

普通 merge-finish 不再直接 drop 自动合并 gate，只报告等待安全结算或当前无记录；非 holder/card 不得有副作用。自动流程无需模型调用 merge-finish，最终状态与宿主 idle 足够。保留该动词以兼容旧提示，但不保留其不安全的早解锁行为。

### 8.3 重启与不确定结果

daemon 启动后、服务 driver/next 前，若发现残留记录，标记 uncertain，禁止任何新合并，不自动重投、不因旧 PID 死亡清除。旧版无记录而有 holder 残留同样阻塞，先人工确认，不假定 flock 空闲意味着旧工具已退出。启动、调度与恢复须严格读取holder：已有文件坏JSON或不可读也是阻塞，不能沿用read_holder的None容错作安全判断。插件普通实现任务不必被整个阻塞，但不能绕过合并安全门。

提供既有动词的离线显式恢复：`merge-finish <card-id> --recover --attempt <action-id> --ack-stopped --state-dir <absolute-path>`。要求先关闭插件、确认相关模型/工具/git 进程均停止、人工核对 Git 与卡片状态；命令实际取得 plugin.lock 与 merge.lock 并 CAS 匹配记录后才清记录，不把人工确认伪装成程序已验证进程停止。卡仍 Running 时拒绝，先由用户判定 done/needs_you；若卡已不存在仍允许核对 attempt 后清理。旧 holder 无 attempt 时使用保留 token `legacy`，仅允许无 journal 且 holder 的 card 匹配；正常 journal 不允许 action_id=`legacy`。

损坏 journal 不提供强制忽略开关：停插件、确认无工具运行后从备份恢复有效记录，再按上述命令处理；不能把坏读当成自动无记录。升级/降级均先排空或人工结算 journal；禁止让忽略 journal 的旧 daemon 接管未结算项目。

这不承诺 exactly-once：结果遗失会保守阻塞，需人工确认，而不是盲目自动重试。保证的是不把不确定执行自动当作未执行。

## 9. 合并指令与发布

实现 objective 禁止“未获授权合并”，不再绝对禁止合并。续跑 objective 自包含说明本轮授权替代旧“只在开发树工作”的任务范围限制，但不替代权限规则。

明确 card ID、源分支、base、开发树、目标树、绝对 state-dir；不使用 `<card-id>` 占位，不依赖 --self。daemon 已持 slot，不再 merge-request；执行目标树检查、合并、冲突处理、项目验证；Git 与验证命令同步等待结束，不留仍能写目标树的后台进程，按事实 state done/needs_you；不 push、不删除源分支。路径/分支使用安全 shell 引用，不能把空 source 送入指令。

同步仓库 skill、README、INSTALL、契约。升级包括宿主、插件二进制、重启旧 daemon、安装目录 skill；热 thread 可能仍记得旧 skill，不能仅靠重新加载 skill 生效。正常流程不删除开发树，否则以后无法冷恢复原 session。

## 10. 历史数据

不猜迁移旧版已覆写的 thread_id；origin_thread_id 不能修复开发绑定。升级说明要求人工核对；无法确认则人工处理。未覆写且保留开发树的卡直接兼容。不扫描用户文件系统寻找“像原会话”的目录，不对未知版本 journal 容错启动。

## 11. 验收判据

以下是待实现要求，不代表测试已通过。计划须逐项映射。

| 编号 | 判据 |
|---|---|
| P1 | 热闲/冷缺观测实现卡交出 resume_append，原 ID/开发 workdir 不变；不 Launch |
| P2 | 忙时不占 gate；busy 竞态零打断、原顺位退队；忙队首可跳过 |
| P3 | 缺绑定 NeedsYou；无 capability 排队并展示原因，零预算仍可展示；独立卡 Launch 兼容 |
| P4 | 峰值1复用 lease；满额不取 gate；pending 保留 lease；一拍一个动作 |
| P5 | CAS 退队/推进；重复过期回报无副作用；先 done 后结果不回退；错 ID 结果阻塞 |
| P6 | journal 先于动作；重启/pending/坏 journal/旧 holder/卡被清理均不自动再合；离线恢复有实际锁与 attempt 校验 |
| P7 | done 但 busy 不解锁；匹配结果之后一拍 idle 且已声明结果才解锁；错卡 merge-finish 不影响 holder |
| P8 | source/state-dir/授权/路径引用正确；不 push、不删开发树、不绕过 sandbox |
| H1 | 热追加单 thread/单 meta；未知 Append 仍失败；热 busy/错 cwd 零副作用 |
| H2 | 冷恢复 mock provider 收到旧 messages/工具结果；配置/usage/anchor/Arc 保留；同 ID 无新 meta |
| H3 | missing/bad meta/log/partial/cwd、writer锁 Busy/I/O、partial升格失败均不发送新轮；空 log 与有效 partial 的合法边界分别覆盖 |
| H4 | 冷 env 注入实际 bash；装配降级不丢 env；观测登记去重；热不改 env |
| H5 | 通知同 thread、有 objective、无 RPC响应/全历史重复灌入；send失败回滚自己的占位并结束通知 |
| C1 | 双端共读契约测试覆盖动作、capabilities、error_code、旧默认与 no-action |
| E1 | 真插件服务 + 真 SessionLauncher + mock provider 串联热/冷两条路径；不得只用 FakeDaemon 代替调度端后宣称集成通过 |

实现阶段至少运行插件 workspace 的 core/runner 测试、宿主 session_driver 与恢复生命周期测试；提交前 yi-agent-rs 下 cargo fmt --all 与 just ci，合并后再跑 just ci。人工验收记录原 thread ID，热/重启后各发 merge，确认原会话收到授权、无新 session；再验证故障后的阻塞与人工恢复。

## 12. 文档阶段证据与限制

本轮没有修改运行时代码。调查基线：原行为测试通过；此前在隔离 worktree 运行 just ci，3991 项测试通过、28 跳过，绑定检查18通过；最终 cargo audit 因当时 zip 8.6.0 命中 [RUSTSEC-2026-0429](https://rustsec.org/advisories/RUSTSEC-2026-0429) 失败，按门禁未提交。文档集成阶段同步main 997c8d4a（zip 9.2.2）后，重新执行 cargo fmt --all 与完整 just ci：退出0，3998项测试通过、28跳过，绑定检查18通过，审计通过（仅配置允许的chacha20撤回版本警告）。这是现有基线的验证，不代表本文新功能已经实现或新测试已经通过。

文档收尾静态检查通过：Markdown链接/围栏、需求编号覆盖、进度计数（kanban 18/20、app-server 79/80）、git diff空白检查。设计级释放模型枚举540种组合、5个崩溃切点，并以3个反例核对旧的早释放规则；plan的shell_quote片段已单独编译并通过5组shell参数往返。以上不是运行时代码的集成测试结果。

复核依据为本分支 e6aab02b 的相关源码；编写计划前核对 main 对相关源码无漂移。实施前须重新核对基线，尤其 app-server 的其他重构，不能把这里的行号当作永久接口。

## 13. 审阅与后续

- [x] 问题与源码、原设计交叉核对，用户选定 B。
- [x] 第一轮：身份/热冷恢复/状态与调用链审阅，纠正宽松恢复和早解锁。
- [x] 第二轮：并发/崩溃/持久化故障推演，补 journal、锁序与离线恢复。
- [x] 第三轮：兼容/权限/环境/数据生命周期审阅，限定 env、sandbox、meta-only 与降级边界。
- [x] 按用户授权进入写计划，并完成 spec-plan 覆盖、接口与负面路径复核。
- [ ] 测试驱动实现并通过全部门禁。

实施计划：[2026-11-03-kanban-merge-original-session.md](../plans/2026-11-03-kanban-merge-original-session.md)。
