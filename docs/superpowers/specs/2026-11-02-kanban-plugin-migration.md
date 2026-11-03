# 看板插件迁移与受管命令契约 v1（G3 门禁材料）

日期：2026-11-03

状态：**等待 G3 人工审阅**。本文是 G3 的输入，不是 G3 的结论。

前置：[G1 宿主协议](2026-11-02-plugin-host-protocol.md)（2026-11-03 批准）、[G2 分发与生命周期](2026-11-02-plugin-distribution-lifecycle.md)（2026-11-03 批准）。

对应设计：[设计](2026-11-02-kanban-independent-release-design.md) §7.1、§7.2、§7.3、§9.3、§11、§12；对应计划：[总计划](../plans/2026-11-02-kanban-independent-release.md)（T05）；执行子计划：[看板迁移](../plans/2026-11-02-kanban-plugin-migration.md)（T21–T23、T25）。

可执行部分：

| 文件 | 作用 |
|---|---|
| `plugins/superpowers-kanban/contract/managed-v1.json` | 17 个 CLI 动词 + 20 个 IPC 方法的 effect 分类 |
| `scripts/plugins/check-contracts.mjs` | `checkManagedContract` + 源码抽取器 |
| `scripts/plugins/check-contracts.test.mjs` | 51 条断言（本条 G3 新增 9 条） |

运行：

```bash
npm ci --prefix plugins/contracts
node scripts/plugins/check-contracts.mjs    # positive: 10, negative: 24, failures: 0
node --test scripts/plugins/check-contracts.test.mjs   # 51/51
```

## 1. 本文冻结什么

- 现有 17 个 CLI 动词与 20 个 IPC 方法的 **effect 分类**，逐个给出理由；
- 分类名单**从源码抽取**并与契约比对，防止"改了源码但契约没跟上"；
- `run` / `manifest` 只能是 `Maintenance`，`merge-request` 不能是 `Read`；
- 状态目录与共享配额域的注入方式（沿用 G2 §8，此处给出看板侧落点）；
- `sessions.running` 的口径：宿主本进程上报，**不是**跨实例全机计数；
- 旧安装探测范围与接管步骤；
- 数据格式版本与「不原地猜迁移」的边界。

## 2. 现状核对（2026-11-03 实测）

### 2.1 抽取结果

```
managed contract (17 cli verbs, 20 ipc methods)
```

| 来源 | 位置 | 数量 |
|---|---|---|
| CLI 动词 | `crates/superpowers-kanban-runner/src/main.rs:144` 的 `USAGE` 第一行 | 17 |
| IPC 方法 | `crates/superpowers-kanban-runner/src/dispatch.rs:100` 的 `dispatch_in` match 表 | 20 |

`dispatch.rs` 里实际有 20 个顶层方法（不是交接摘要里记的 19 个）——多出的是 `settings.write` 之外我此前漏数的 `board.release`。**以抽取器输出为准**，这正是为什么不手抄。

### 2.2 两份名单必须由机器维护

契约里的名字若与源码不一致，`checkManagedContract` 会双向报错：

- 源码有、契约无 → “no effect classification”（新动词没被归类，可能被错误授权）；
- 契约有、源码无 → “no longer exists”（契约漂移）。

抽取器在拿不到任何动词时**返回空数组并由检查报错**，而不是静默通过。这条有独立测试（`the source extractors fail loudly rather than returning empty`），因为一个正则失配就永远绿的检查比没有检查更糟。

**变异验证**（已做）：从契约删掉 `purge` → 检查立刻 `FAIL` 并指名 `"purge" exists in the source but has no effect classification`；正样本计数从 10 掉到失败，`failures: 1`。恢复后回到 `failures: 0`。

## 3. CLI 动词 → effect 映射

| 动词 | effect | 理由 |
|---|---|---|
| `run` | Maintenance | 宿主 supervisor 拉起后端，不是业务入口 |
| `manifest` | Maintenance | 打印 supervisor 清单供宿主安装消费 |
| `add` | Write | 投递 spec+plan 对 |
| `add-prequeued` | Write | 记录一张想法 |
| `promote` | Write | 提升 pre-queued → queued |
| `add-merge` | Write | 投递合并卡 |
| `merge-request` | **Write** | 改 `merge_queued` 状态并占合并锁，**不能是 Read** |
| `merge-finish` | Write | 释放/清除合并槽位 |
| `merge-cancel` | Write | 退出合并队列 |
| `done` | Write | 完成并归档 |
| `state` | Write | 模型声明状态；daemon 之外唯一写状态入口 |
| `archive` | Write | 归档终态卡片 |
| `purge` | Write | 永久删除已归档记录（destructive） |
| `list` | Read | 只读列举 |
| `on` | **QuotaWrite** | 见 §4.2 |
| `off` | **QuotaWrite** | 见 §4.2 |
| `workdir` | Read | 打印看板工作目录 |

## 4. 两个需要说明的判定

### 4.1 `merge-request` 必须是 Write

设计 §4 的验收要求原文是「`merge-request` 不能作为 Read」。这条在 checkrer 里是显式规则，不是 schema 能表达的（名字与 effect 都合法）。理由：它把卡片改成 `merge_queued` 并占用项目合并槽位，是一次真实的状态写入。把它标成 Read 会让"只读授权"的项目悄悄拿到合并队列的写权限。

### 4.2 `on` / `off` 归 QuotaWrite，不并入 Write

这两个动词只写项目偏好层的一个布尔值，看起来像普通 Write。但它的效果是**开始或停止产生新的自动任务**。

G1 §5.1 已经把「发起自动任务」（`Launch`）列为独立授权项，理由是"用户批了 Write 不等于同意系统自己开跑"。`on` 的效果是解除这个闸门，属于同一类。因此：

- 判定为 `QuotaWrite`（G1 里"修改影响整机运行配置"的那一项），不并入 `Write`；
- 依据：设计 §7.1 把"同用户整机并发窗口/峰值配置"列为共享域，开关决定是否产生自动任务，与配额同属"影响整机是否运行"的配置。

**这是判断而非事实**，所以列在 §9 请审阅者确认。

## 5. IPC 方法映射

| 方法 | effect | 说明 |
|---|---|---|
| `list` | Read | |
| `enqueue` | Write | |
| `enqueue_prequeued` | Write | |
| `promote` | Write | |
| `enqueue_merge` | Write | |
| `switch.read` | Read | |
| `switch.write` | QuotaWrite | 与 §4.2 同源 |
| `driver/next` | **Launch** | 每 tick 可回一个 launch 动作 |
| `board.next_launch` | **Launch** | |
| `board.mark_running` | Launch | |
| `board.mark_terminal` | Write | |
| `board.release` | Write | |
| `merge_request` | Write | |
| `merge_finish` | Write | |
| `merge_cancel` | Write | |
| `card.state` | Write | |
| `lifecycle/create` | **Resident** | 登记常驻服务 |
| `lifecycle/remove` | Resident | |
| `settings.read` | Read | |
| `settings.write` | **QuotaWrite** | settings 含并发窗口与峰值 |

`board.mark_terminal` 与 `board.release` 判为 Write 而非 Launch：它们**结束**一次运行，不产生新的。把结束类动作升级成 Launch 会让「只批了 Launch 的项目」在结束时被拒，反而破坏正常收尾。

## 6. 状态目录与共享配额的注入

### 6.1 状态目录

- 由宿主作为显式运行上下文传入（`--state-dir`）。
- **不能默认成 worktree cwd**（设计 §7.3 明写）：那会让会话写到另一块看板。
- 项目数据目录来自授权项目的插件数据目录，安装/更新不删除（设计 §7.1）。

### 6.2 共享配额域

沿用 G2 §8.1 的实测结论：

- 路径 `$HOME/.yi-agent/superpowers-kanban/leases`，flock 槽位；
- 与实例根**分开**，不通过重写 `HOME` 隔离；
- 配置与租约必须用同一配额域；
- 无法定位共享域时**阻止新自动任务**，不静默创建独立预算池（`service.rs:490`/`:682` 实际行为）。
- `lease.rs:47` 的注释与代码矛盾，T13 修注释（G2 §8.1 已记录）。

### 6.3 `sessions.running` 的口径

`driver-protocol.json` 的注释已经写明：`sessions.running` 是宿主**本进程**此刻的全机运行会话数，旧宿主不发时插件退化为看板内口径。

**它不能直接当跨实例全机计数。** 设计 §7.1 要求整机窗口跨实例共享，若每个实例都拿自己进程的数字去比总上限，两个实例各自看到"还有余量"，合计就会超。

正确口径（交由 T13 实现，G3 只冻结"不能用错值"）：

- 已登记活 owner 的租约合计 + 本进程上报，且**同一租约不双扣**；
- 老宿主不发字段时不假设为 0，而是退化为看板内口径并明确标注统计范围。

## 7. skill 与受管命令

### 7.1 skill 来源

- 从版本目录发现插件拥有的 skill，**不复制**到用户全局目录（设计 §7.2）。
- 只对已授权项目的**后续**会话提供。旧会话保留原上下文，不热替换（设计 §9.3）。
- 同名冲突：显示路径与来源，不覆盖用户文件；冲突解决前阻止自动看板任务，普通聊天与只读查看照常。

### 7.2 受管命令绑定

- 宿主提供通用插件 CLI 启动能力，通过受管解析入口定位程序，**不依赖用户 PATH、不改全机 PATH**。
- 启动参数必须能表达：插件 ID、已绑定项目、版本代次、原样参数数组（不拼 shell 字符串）。
- 版本切换/停用/卸载/重连到不同宿主后，旧绑定被拒绝并返回"需要新会话"的明确错误，**不悄悄转发**给新程序。

### 7.3 不宣称安全边界

版本绑定保证受管调用的一致性，**不阻挡**模型用普通 shell 直接运行历史二进制或改文件。这与 G1 §5.3 的只读边界同理：隔离层只覆盖经宿主桥的调用。UI 文案不得暗示它是沙箱。

## 8. 旧安装探测与接管

### 8.1 探测范围（不许递归扫盘）

只查用户提供或实例已登记的项目，以及已知技能根。检测项：

1. PATH 上的旧 `superpowers-kanban` 程序；
2. 项目 supervisor 登记；
3. 用户级 / 项目级 skill 副本；
4. 常驻登记；
5. 原功能开关。

列出**来源与关联项目**。**不执行未知旧程序来证明身份**，也不把已有开关解释为新能力授权。

### 8.2 接管步骤（用户显式选择后）

```text
获取并验证正式包 → 备份旧记录 → 停止新任务并排空旧操作
→ 停止旧受管进程 → 检查数据兼容性与同名 skill 冲突
→ 用户确认项目/后台/全局配额权限 → 原子建立新登记并切换 → 健康检查
```

失败时可恢复的旧记录要恢复；**未知格式不原地猜迁移**（设计 §11）。

### 8.3 保留与不保留

**保留**：卡片 ID、状态、队列次序、项目设置、会话跳转关联。

**不保留**：旧全机配额与活锁**不复制**成每实例独立池（设计 §11 + G2 §8.1）。

**不动**：用户自定义 skill、PATH 上用户安装的旧程序（只说明已不再被托管）。插件自有旧副本仅在确认归属且用户同意后归档。

接管后同一项目**不能同时运行新旧两个看板进程**。

### 8.4 旧 RPC 不得冒充下载

旧 `plugin/install` 的「注册并启用」语义**不得**在新 UI 里冒充下载。三个动作分开：包安装、项目启用、常驻授权。不支持的旧客户端收到明确错误/升级提示，不能被旧 RPC 绕过授权。

### 8.5 迁移后删除的暗门

旧看板专属 UI、TUI、系统 skill、实验开关入口与帮助文本，迁移完成后删除或迁为通用插件入口。**不保留**可绕过新管理流程的入口。

## 9. 需要审阅者决定的事

1. **`on` / `off` 归 `QuotaWrite`**（§4.2）是否接受。替代方案是并入 `Write`，好处是授权项更少；坏处是"批了写卡片"就等于"同意系统自己开跑"，与 G1 把 `Launch` 独立授权的决定矛盾。
2. **`board.mark_terminal` / `board.release` 归 `Write` 而非 `Launch`**（§5）是否接受。替代方案是统一归 Launch，但那会让只批了 Launch 的项目在**结束**运行被拒。
3. **`sessions.running` 不做跨实例直接相加**（§6.3）是否接受。替代方案是把各实例上报值直接加总，但那在两个实例都接近上限时会突破总配额。
4. **旧安装探测只查已登记项目与已知技能根，不递归扫盘**（§8.1）是否接受。替代方案是全盘扫描，代价是慢且会碰用户没授权的目录。

## 10. 本文未证明的事

- **没有实现。** 分类是契约，`on`/`off`/`lifecycle/*` 的真实授权检查在 T13/T16/T21 才落地。
- **抽取器依赖源码形状。** 它认 `USAGE` 首行与 `dispatch_in` 的 8 空格缩进 match 臂。若有人把这两个结构重写字法，抽取会返回空并由检查报错——**会红，不会静默通过**，但仍需人工修正正则。
- **20 个 IPC 方法里没有一条被真实权限检查覆盖**，只是被分类。
- **旧安装探测没有在真实旧安装上跑过**（G0 只验证了 macOS arm64 的空环境）。
- **共享统计的聚合算法未实现**，§6.3 只冻结口径。
