# 看板会话存活误报：完整热会话快照与观测语义

日期：2026-11-03
状态：已按用户要求完成三轮设计复核并修订；实施计划待执行，修复尚未实现
分支：`docs/kanban-stale-observation`

## 1. 目标与范围

修复 Superpowers 看板的 running 卡片提示“卡片写着 running，但会话已经不在了”，而同 ID 会话实际已被当前宿主加载的误报。同时消除“未加载”等同于“历史已不存在”、轮询时间等同于“最后活动”的文案错误。

范围：通用宿主 driver 快照、插件快照解释与展示旁路、桌面归一化/角标、CLI 与随包 skill 说明、契约与回归测试。本文只规定修复需求，不是实施计划。

非目标：不自动恢复或启动会话，不代模型迁移卡片状态，不删除/改写历史，不添加新的卡片状态，不调整并发窗口，不扫描磁盘判断正在运行，不新增跨宿主实时运行汇总，不修改合并 attempt 的安全释放规则，不重做插件发布体系。

## 2. 证据与根因

### 2.1 现场反证

2026-11-03 15:56–15:59（本机 UTC+08:00）只读取项目看板、会话文件及进程文件描述符，未改卡片或运行时数据：

| 卡片 | 执行会话 ID | 看板状态 | 观测 |
|---|---|---|---|
| shared-daemon-lifecycle-and-upgrade | `thread-79375bcc-0757-49de-966d-86fa5d74aba7` | running | alive=false，最后 alive 为 08:35:13.981658 |
| kanban-independent-release | `thread-c652d41f-d143-4a19-aa27-f299fa6cac52` | running | alive=false，最后 alive 为 08:35:13.981658 |

两条 ID 均来自 `board.json` 的 `thread_id`，不是 `origin_thread_id` 或标题相似的会话。安装的 `/opt/homebrew/bin/superpowers-kanban list` 对两卡实际输出 `[stale]`。

各自 `card.workdir/.yi-agent/threads/<thread-id>.meta.json` 可解析，ID、cwd、driver_project、driver_ref 与卡片绑定一致。两条 jsonl 分别为 9,001,067 和 5,523,520 字节，partial 也存在。此证据证明持久化数据存在，不单独证明可安全恢复或当前有 turn 在跑。

更强的热会话反证：`lsof -p 1291` 显示当前桌面 sidecar 持有这两条 ID 对应的 `.lock` 描述符，分别为 fd 30w、69w。结合 ThreadSession 的 writer-lock 生命周期，说明不是只有侧栏缓存/磁盘历史，而是当前宿主已经持有这些会话。该证据不等于它们正在调用模型或执行工具。

进程已运行约 6 小时 44 分钟，启动晚于上述最后 alive 时间；新启动的 side-conversation 和 tui-alternate-scroll 两卡则 alive=true。该分布支持重启丢失登记的触发链，但未保留重启前后的原始 RPC 抓包，不声称确定了 08:35 后具体哪一次退出事件。

### 2.2 源码链路（调查基线 a44fbe5a）

1. `yi-agent-rs/crates/yi-agent-app-server/src/server/transport/mod.rs`：serve 的 `drivers: HashMap<(PathBuf, String), DriverState>` 是主循环局部量，启动时为空。
2. `src/session_driver.rs::driver_step`：只有该 driver 的 Launch / ResumeAppend 成功回报会登记 DriverThread；不是所有加载会话的发现机制。
3. `src/server/driver/tick.rs::driver_tick`：只遍历已有 `state.threads`，用 `threads.contains_key` 刷新 alive。即使一个旧会话已被普通 RPC 恢复到宿主 threads 表，重启后空的 driver 记账仍不会发现它。
4. `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs::write_observations`：本拍未提及的旧观测 ID 一律 observe(false)，保留上一次 true 的时间。
5. `src/observation.rs::stale`：running + 有绑定 ID + 观测 alive=false 即 stale。
6. `desktop/src/components/SuperpowersKanbanCard.tsx`：把 stale 翻为“会话已经不在了”，把 `lastAliveAt` 显示为“最后活动”。但是 `ObservationFile::observe(true)` 每次轮询都会刷新该时间，即使会话一直 idle。

因此有三个需要明确区分的事实：持久化会话数据是否存在、当前宿主是否已加载会话、当前会话是否正在运行 turn。现有 alive 本质上是第二项，却来自不完整的 driver 登记子集，并被文案解释成第一项。

### 2.3 最小复现及旧测试漏洞

宿主 Launch 会话并记账 → 保存历史 → 重启使 drivers 为空 → 普通 thread/resume 恢复同 ID → driver tick 只刷新空子集 → 插件把旧 ID 记为 false → running 卡显示 stale。

不需真实 LLM 或真的重启进程：测试在真实 Host fixture 中保留/恢复 ThreadSession，清空 DriverState，再 tick，即可复现关键断点。不得仅测试 `threads_payload` 的手造输入。

已有 `a_thread_that_left_the_host_table_is_reported_dead_on_the_next_tick` 覆盖“已登记 ID 离开热表”，但未覆盖“drivers 空、热表仍有原 ID”。插件旧测试明确把空快照解释为全体 stale，证明误报并非单纯 UI 渲染缺陷。

## 3. 方案比较

- A：只修文案。最小成本、旧数据立即不再声称历史消失，但已加载会话仍会误报，租约观测依然不完整；不作为完整修复。
- B（推荐）：通用宿主提供完整已加载会话快照，插件按自己的绑定 ID 匹配；展示使用加载语义而不是存在语义。无需宿主读取 board.json，也无需推断 driver_ref 属于哪个插件。
- C：靠 metadata 重建每个插件的 DriverState 或扫描历史判断 alive。driver_project/ref 不提供可靠的多插件所有者标识；历史存在也不证明热运行。为修复展示引入持久化归属迁移或冷恢复不合算，不采用。

采用 B 编写实施计划。用户已授权在发现并修正文档问题、复核无未解决问题后进入 plan；本次不授权实施。

## 4. 不变量

1. 会话观测本身不改卡片 `state`、`thread_id`、`origin_thread_id`、`workdir`、detail 与终态时间；同一 tick 中已有动作回报与授权调度的合法写入保持原规则，不把本条误解为禁止正常回报落账。
2. 宿主只提供通用 thread 事实，不认识 card/kanban，不读取项目看板或从 opaque driver_ref 猜插件归属。
3. idle 不等于 unloaded，也不等于任务完成；持久化历史不等于 loaded。
4. 当前宿主热表确实存在的绑定 ID，无论由 driver 还是普通 RPC 装配，必须在下一次成功 tick 后展示 loaded。
5. 新观测不能把未查询、字段缺失、查询失败或旧缓存当成最新死亡证据。
6. alive/idle 的调度事实与 UI 的不确定性必须分开；不得把展示 unknown 偷偷转成“已停止”，不得因此释放未结算 merge attempt。
7. 观测为宿主进程本地视角，不证明其他宿主进程没有执行同一会话；不作跨进程停止保证。

## 5. 通用宿主契约

在 `driver/next` 请求增加可选 `loaded_threads` 数组：

```json
{
  "threads": [{"thread_id":"thread-known","alive":false,"idle":false}],
  "loaded_threads": [{"thread_id":"thread-known","idle":true}],
  "results": [],
  "sessions": {"running":0}
}
```

- `loaded_threads` 是当前宿主 `HashMap<String, ThreadSession>` 的完整快照，不是 DriverState 的子集。数组出现即声明完整性；空数组表示当前热表为空，字段缺失表示旧宿主/无完整快照能力。
- 每项只有 ID 与 idle，不传 messages、路径、标题、权限或 secrets。不需要新增插件所有者 metadata。
- idle 与现有口径相同：Idle 且无 active_turn_id。AwaitingApproval、Running 或仍持 active_turn 的会话不是 idle。
- threads、loaded_threads、sessions.running 在同一 tick、创建 launcher 前从同一主循环状态采样；动作后的变化下一拍上报，不伪造“实时绝对一致”。
- 现有 threads/results 继续承担动作登记与结果回报；不伪造 Launch 结果、不重放旧 action、不为手工恢复添加 pending action。
- 契约解析：缺字段为兼容；显式 null、非数组、缺 ID/idle、空或纯空白 ID、错误类型或重复 ID 为无效快照，该拍拒绝且返回清楚错误，不按空数组处理。不 trim 后重新匹配 ID，不解释其插件归属；非空 ID 原样比较。拒绝发生在消费 results 前；宿主保留 results 重试。
- 保持既有 IPC 帧大小限制（插件 socket 为 1,048,576 字节）。超限走现有传输错误与重试路径，不静默截断数组、分批冒充完整快照，也不省略字段退回旧宿主语义。不增加分页协议或提高帧上限；必须记录可定位的错误。
- 宿主只向既有获授权的通用 session_driver 通道发送这些宿主内存 ID，不增加远端 RPC 权限或新的公开会话查询 API。

两端共读 `contract/driver-protocol.json` 锁住字段和旧载荷兼容。旧 runner 忽略新增字段时行为不变，因此完整修复须宿主与 runner 都升级；仅升级 UI 能修文案，不能宣称热会话误报已修。

## 6. 插件解释、旁路与调度边界

### 6.1 一次归一化，多个消费者

插件只匹配 board 中绑定的非空 ID、仍有效的 pending/merge attempt 绑定及匹配本进程 pending action 的成功回报 ID。包含已归档但仍在 board 的卡，以免漏掉其热会话租约；不使用 origin_thread_id。既有旁路只为这些相关 ID 保留最后观测时间，已 purge 且无在飞绑定的记录在成功快照写入时清理。不把全宿主无关 ID、未经 action_id 匹配的任意 result ID 或旧旁路里的孤儿 ID 重新扩散为观测对象。不额外解析原始会话历史。

处理顺序：先完成所有请求解析、检查租约目录、读取用于限定 ID 的 board/attempt/pending 及相关结果身份；归一化完整事实并原子保存旁路，成功后才推进 tick 序号、消费回报、执行现有 gate 结算和调度。board/attempt 的相关 ID 读取错误也须在消费回报前拒绝；没有 board 文件可按首次空看板处理，坏/不可读文件不能按空看板处理。本次不重做一般回报写盘的事务机制。

这保证本拍 next_launch/adopt 的租约对账读到已成功保存的同一批事实，不依赖一份未落盘的内存新事实。匹配 pending 的成功 Launch/ResumeAppend 回报 ID 在保存前纳入相关 ID，即使它尚未写回 board；完整快照缺该 ID 时以当前完整快照为准，不能凭成功结果伪造当前 loaded。新发出的动作在本拍观察保存之后产生，沿用 pending 的保留租约规则，下一拍才观察其新会话。

有效完整快照出现时：

- ID 在 loaded_threads 内：有效 ThreadFacts 为 alive=true、idle=其值；覆盖 driver threads 中对同 ID 的陈旧 false。
- 已知绑定 ID 不在完整快照：有效事实为当前宿主 alive=false、idle=false，不用旧 idle 推断已完成。
- 以有效事实供现有观测/租约对账及 busy 判断共用，避免 UI 已修而租约仍看旧子集。现有全机 running 与 lease 去重策略不改，不能给同一卡片追加第二份租约。
- 合并 gate/attempt 的安全释放继续要求原有 accepted、绑定不变、后续明确 alive+idle 等全部条件；未加载不证明工具停止。租约对账必须保护尚未清理的 pending/accepted/uncertain attempt 对应卡的已有单份租约，不能因该 ID 不在完整快照而提前释放；不拿第二份租约。每次有效 tick 在旁路保存及回报结算后执行既有租约补账，即使 sessions.running 导致 budget=0 也不能漏掉热卡补账。gate 结算成功清理 attempt 后，恢复既有租约口径，不把本条扩展成跨进程停止证明。

旧宿主缺完整快照时：显式 threads 内的 ID 仍按旧 alive/idle 执行既有调度默认值（缺 alive 默认 true、缺 idle 默认 false），但只有显式 boolean alive 才构成 loaded/not_loaded 展示证据；默认 true 不刷新 last_seen_alive_at，也不伪造 observed_at。未提及 ID 的展示为 unknown，不再把缺字段伪装成明确未加载。为控制兼容范围，旧版遗漏 ID 的租约行为保持原规则，展示 unknown 不作为额外释放理由；不声称修复旧宿主遗漏 ID 的全局配额漏洞。新宿主完整快照才提供本次修复的有效事实。不重复领租约、不改变 sessions.running 口径与 merge attempt 安全门。

### 6.2 展示数据

旁路增加可选观察信息，保留旧 alive 与 last_seen_alive_at 字段供兼容：

- `session_observation`：loaded / not_loaded / unknown。
- `observed_at`：这次有效宿主快照或显式单 ID 报告的时间。
- `last_seen_alive_at`：最后观测到已加载的时间，仍是轮询时刻，不是最后消息/工具活动时间。

旧 observations 只有 alive、没有新观察信息时，展示降级 unknown，直到收到有效新报告；不得把历史 false 直接升级为当前 not_loaded 的证明。新格式中的未知枚举、缺失/无效 observed_at 也只影响该 ID 的展示降级，不丢弃其他有效 ID；旧 alive 与旧时间字段保持可读取供既有调度。unknown 的 observed_at 为 null，不把“这一拍遗漏”当有效报告时间；last_seen_alive_at 保留真实历史值。持久化旁路仍原子 temp+rename，不写 board.json。

只有完整快照或显式 boolean alive 报告才更新新展示信息。加载态和最后已加载时间都来自同一报告；缺 alive 的保守默认不算报告。没有非空执行 thread_id 的 running 卡不显示加载角标，不能拿来源讨论会话判断执行会话加载状态。

插件 list 继续提供旧 `stale` 与 `last_alive_at`，新增每卡 `session_observation` 与 `observed_at`。新完整观测下 stale 仅在 running + not_loaded 为 true；unknown 不声称已不存在。旧消费者无需理解新字段也可继续显示保守角标。

查询未成功不推进观测时间；展示使用“截至 <观测时间>”而不是声称此刻实时状态。不新增任意 TTL 判死机制，不跨宿主合并观测。

## 7. 桌面、CLI 与 skill

只对 running 卡展示加载异常；非 running 卡未加载是正常情况。

- loaded：不显示原 stale 警告，不把 idle 转成 needs_you/awaiting_merge。
- not_loaded：角标 label“会话未加载”，提示“截至 <观测时间>，当前宿主未加载该会话；会话历史可能仍存在。卡片仍为 running，需检查实际进度。”
- unknown：中性提示“会话观测未知”；缺时间不编造“刚刚”。
- 只有旧 stale=true、没有新字段：提示“当前宿主未观测到该会话；会话历史可能仍存在”，不能继续显示“会话已不在”。
- 最后时间标签统一“最后观测到已加载”，删除“最后活动”。采用包含日期的可读时间，避免隔天记录只显示 08:35；无效 ISO 不渲染伪时间。
- 原会话链接保持同一 thread_id，可手动打开。一次普通 resume 成功后，下次成功 tick 清除 not_loaded/unknown，不重新建卡或换 ID。
- CLI 保留 `[stale]` 供旧用户识别，追加其当前宿主未加载语义；unknown 使用 `[observation unknown]`。只有历史旧标记时也明确“未观测到”，不声称删除。
- 随插件发布的 skill、INSTALL.md 及类型注释同口径，不建议用户为了清角标把 running 改成 done，不修改并发窗口，不手写旁路文件。

## 8. 错误与恢复

- 无效新快照整拍拒绝且不消费回报、不写旁路、不派发新动作，返回可诊断错误；下一拍可重试。
- 查询错误保留已有旁路及其历史时间，不刷新成最新未加载。
- observations 缺失/旧格式按 unknown 展示；坏文件沿用只读容错但不能显示明确死亡。旁路写失败须在消费回报前返回错误：保留旧文件、tick 序号、pending 和已有租约，不释放 gate、不派发动作；下拍重复 results 可正常消费。不得只 log 后继续从旧文件对账。
- 不为修复本 bug 把冷会话都恢复进内存；用户主动打开后由正常恢复路径装配，再由完整快照发现。
- 多插件、多个项目同宿主时，只处理自身绑定 ID，不将其他项目会话关联到当前卡，不重复落盘无关 ID。
- 再次卸载热会话后，下一拍明确 not_loaded；历史仍存在时文案依然不能说删除。

## 9. 验收矩阵

以下均为待实现验收，不能把当前绿的旧测试当作修复证据。

| 编号 | 场景 | 完成判据 |
|---|---|---|
| K1 | drivers 空，热表有原卡 ID | 真 driver_tick 发送完整快照；插件 list 为 loaded、stale=false；ID/卡片字段不变 |
| K2 | 实际普通 thread/resume 恢复同 ID | 从真实 ThreadStore 恢复并 tick，无 Launch、无额外模型回合，角标清除 |
| K3 | driver 子集 alive=false，完整快照同 ID 已加载 | authoritative 完整快照胜出，观测 true，idle 使用完整快照 |
| K4 | idle 已加载 / busy / AwaitingApproval | 全部 loaded；busy 不被抢占，idle 不改变卡片状态，running 数沿用旧口径 |
| K5 | 热表移除原 ID，历史仍在 | not_loaded，提示不声称历史消失；原线程链接保留 |
| K6 | 宿主重启后尚未恢复 | 有新完整空快照才 not_loaded；只有旧 false 或缺报告时 unknown |
| K7 | 多项目/多插件和普通会话 | 自身 ID 正确匹配，不保存无关 ID，不猜 opaque driver_ref 所属插件 |
| K8 | 旧宿主/旧 runner/旧 UI 混装 | 协议不崩溃、不丢 results；新 UI 连接旧 runner 时旧 stale 只说未观测到；旧 UI 的硬编码提示无法由 runner 修复，诚实文案须 UI 升级，不承诺旧 UI 满足新文案 |
| K9 | 非法/重复/缺字段完整快照 | 拒绝且不消费 results、不派发、不覆盖有效旁路；修正后正常重试 |
| K10 | 查询失败、旁路损坏、时间缺失/非法 | 不伪造当前报告、死亡或日期；unknown/历史观测语义明确 |
| K11 | 租约与合并回归 | 热卡重新被观测后复用/补领既有单份 lease；无重复额度；未加载/unknown 不解锁不确定 merge attempt |
| K12 | timestamp 每拍刷新但会话一直 idle | 页面只称最后观测到已加载，不称最后活动；跨日时间显示日期 |
| K13 | 修复对照 | 删除完整快照生产或消费逻辑时 K1–K3 至少对应红；恢复后全绿，证明不是纯 mock 自洽 |
| K14 | 旁路写失败或相关 board/attempt 无法读取 | 在消费前拒绝，pending/results/lease/tick/gate 不变，无派发；故障排除后同回报仅消费一次 |
| K15 | 旧报告缺 alive、新展示字段局部损坏 | 不伪造 loaded 或最新已加载时间；只降级该 ID 的展示，其他有效 ID 与既有调度数据不丢 |
| K16 | 同拍匹配成功结果、孤儿/来源 ID、归档与 purge | 成功结果 ID 在绑定前被正确观察；陌生结果不扩散；归档热卡仍对账；purge 后无在飞引用才清理 |
| K17 | 超过既有 IPC 帧限制 | 明确传输错误，完整快照不截断/不省略；回报保留、无动作、无伪 not_loaded |

测试入口：

```sh
cd yi-agent-rs
cargo test -p yi-agent-app-server --lib kanban_observation_
cargo test -p yi-agent-app-server --lib session_driver
cd ../plugins/superpowers-kanban
cargo test -p superpowers-kanban-runner
cd ../../desktop
npx vitest run src/lib/superpowersKanbanState.test.ts src/components/SuperpowersKanbanCard.test.tsx
npx tsc --noEmit
```

新增用例可统一使用 kanban_observation_ 作为测试检索前缀；这是测试命名，不是宿主实现中的看板分支。实施时提交前及合并后均需 `cargo fmt --all`、`just ci`；插件独立 workspace 单测另跑，不以主仓库 CI 代替。

真机验收：使用隔离实例；启动卡片后重启宿主，先看到未加载语义，再手动打开原会话；下一次成功 tick 后 loaded、无误报；移除热会话而保留历史，验证只提示未加载。不要在生产卡上自动修改状态或启动新的模型 turn 进行验证。

## 10. 调查复核与交付边界

现场复核步骤：从项目 board.json 获取 running 卡的 thread_id；读取同 ID observations；只在 card.workdir 的精确 ThreadStore 核对 meta 和文件大小；从 ps 确定 sidecar PID，再用 lsof 核对该 ID 的 writer lock；用安装的 CLI list 交叉检查 stale。无需输出历史内容、模型配置或真实 relay 参数。

本轮交付：根因、完成复核的修复 spec、对应实施 plan 与未修复进度记录。未修改实现、未声明功能已修复，未创建新看板卡。用户本轮仅授权复核后写 plan，实施另行确认。

## 11. 多轮复核记录

- 第一轮（对照实际接线）：修正旧 UI 混装无法兑现文案的承诺；明确观测不变量不阻止正常回报写卡；收敛相关 ID 范围。
- 第二轮（失败与时序）：将旁路保存前移到消费回报之前，避免失败后租约读取旧文件；明确同拍成功回报 ID 的匹配、旧 alive 默认值与展示证据的区别、局部坏字段降级。
- 第三轮（反例与可执行性）：核对真实普通 RPC 恢复测试路径、无执行 ID 卡、归档/purge、超大完整快照；补齐 K14–K17。复核无未解决的设计问题，采用 B 进入计划；这不是代码通过或真机已修复的声明。
