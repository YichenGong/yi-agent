# 托管进程选择器公开契约：故障链路取证记录

日期：2026-11-03
对应 spec：[2026-11-03-process-selector-contract-design.md](../superpowers/specs/2026-11-03-process-selector-contract-design.md)
对应 plan：[2026-11-03-process-selector-contract.md](../superpowers/plans/2026-11-03-process-selector-contract.md)（Task 1）
取证 worktree：`.worktrees/kanban/2026-11-03-process-selector-contract-design-2026-11-03-process-selector-contract`
取证基线：`509fed0c`

本文只记录与工具签名有关的脱敏事实：工具名、description、properties、required、strict 与实例身份。
**未记录**任何请求头、密钥、cookie、完整对话或进程输出；实例配置中的 `api_key` 已脱敏。

全文严格区分两类陈述：

- **【观察】** 由命令输出或文件内容直接得到，可复现。
- **【推断】** 由观察组合得出，未经第一手验证。

---

## 0. 取证基线与方法

| 项目 | 结果 | 性质 |
|---|---|---|
| `git status --short` | 空（工作区干净） | 观察 |
| `git branch --show-current` | `kanban/2026-11-03-process-selector-contract-design-2026-11-03-process-selector-contract` | 观察 |
| `git rev-parse HEAD` | `509fed0ca920518a4a4c87cb5805e00c982fa31d` | 观察 |
| `git rev-parse --git-dir` | `…/yi-agent/.git/worktrees/2026-11-03-process-selector-contract-design-2026-11-03-process-selector-contract` | 观察 |
| `git rev-parse --git-common-dir` | `…/yi-agent/.git` | 观察 |

方法约束（与 plan 的 Global Constraints 一致）：

- 只搜索当前项目目录，以及用户/控制器明确给出的实例路径 `/Applications/yi-agent.app`。
- 未启动任何新进程去"猜"服务版本；未重启、未替换、未部署正在使用的实例。
- 未发起任何真实 LLM API 调用。
- 运行实例的 schema **没有被导出**；凡是未经授权诊断入口取得的内容，一律记为"无法取得"，不用仓库生成的 schema 冒充实例导出。

---

## 1. 实例身份

| 必记字段 | 记录 | 性质 |
|---|---|---|
| 实际入口/路径 | `/Applications/yi-agent.app/Contents/MacOS/yi-agent app-server --listen stdio:// --relay <已脱敏>` | 观察 |
| 进程身份 | pid 1291，启动时间 `Tue Nov 3 09:11:56 2026` | 观察 |
| 版本来源 | `Info.plist` 的 `CFBundleShortVersionString` = `CFBundleVersion` = `0.1.13` | 观察 |
| 版本与源码对应 | 仓库 `yi-agent-rs/Cargo.toml:22` 为 `version = "0.1.13"`，由 `4378cc6b chore: bump 0.1.13`（2026-11-01）引入，是 HEAD 的祖先 | 观察 |
| 二进制时间 | `Contents/MacOS/yi-agent` mtime = `2026-11-01 22:53:46`（同目录 `desktop` 同时间） | 观察 |
| 可验证的构建提交 | **`f4a2b433`** —— 实例自报 `yi-agent 0.1.13-dev+f4a2b433`（`/Applications/yi-agent.app/Contents/MacOS/yi-agent --version`），该 sha 是 HEAD 的祖先。`build.rs:68-80` 在非 release tag 构建时嵌入 `rev-parse --short HEAD` | 观察 |
| 与 checkout 的关系 | checkout 是该二进制的后续状态（`f4a2b433..HEAD` 有提交），但 `git diff f4a2b433..HEAD -- yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs` **为空**：进程工具源码逐字节相同 | 观察 |

**二进制与 checkout 的字符串比对【观察】**（`strings <binary> \| grep -Fc <text>`）：

| 字符串 | 命中次数 | 含义 |
|---|---|---|
| `exactly one of process_id or name is required` | 1 | 运行时拒绝文案（`selector_from_args` 的 `(None, None)` 分支）在实例中存在 |
| `provide exactly one of process_id or name` | 1 | 运行时拒绝文案（双选择器分支）在实例中存在 |
| `Read buffered stdout and stderr from a managed background process.` | 1 | 与 checkout 当前 `ProcessReadTool::description` 逐字相同 |
| `Kill a managed background process by process_id or name.` | 1 | 与 checkout 当前 `ProcessKillTool::description` 逐字相同 |
| `Managed process id returned by process_start or process_list.` | 1 | 与 checkout 当前 `process_id` 字段描述逐字相同 |
| `Unique managed process name.` | 1 | 与 checkout 当前 `name` 字段描述逐字相同 |
| `Provide exactly one of process_id or name; omit name when using process_id.` | 0 | Task 2 计划引入的新字段说明**不在**实例中 |

【观察】实例自报构建 sha `f4a2b433`，且该 sha 处的 `process/tools.rs` 与 HEAD 逐字节相同（`git diff` 为空）。
【观察】实例的 `process_read` / `process_kill` 工具描述与字段说明和 checkout 当前源码**逐字一致**，且不含 Task 2 的目标文案。
【结论，证据已足够】实例的进程选择器**源端 schema 必然与 checkout 相同**：`type` + `properties`，**没有** `required` 键。这不再是推断，而是由构建 sha + 文件逐字节相等推出的确定结论。
【观察】`process/tools.rs` 全历史仅 3 次提交（`b2b3a7e3` → `62f1ddff` → `2d441457`），`2d441457` 之后没有任何提交再触碰该文件；`git log 2d441457..HEAD -- …/process/tools.rs` 为空。因此从 `2d441457` 到 HEAD 的任何构建，进程选择器 schema 都**不含** `required` 键。

---

## 2. 源端定义

### 2.1 仓库源码（已取得，可复现）

取得途径：直接读 `yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs`（基线 `509fed0c`，工作区干净）。

| 项目 | `process_read` | `process_kill` |
|---|---|---|
| 反序列化结构体 | `ProcessReadArgs`：`tools.rs:172-177` | `ProcessKillArgs`：`tools.rs:246-249` |
| `process_id` 类型 | `Option<String>`（`:173`） | `Option<String>`（`:247`） |
| `name` 类型 | `Option<String>`（`:174`） | `Option<String>`（`:248`） |
| 其他字段 | `cursor: Option<u64>`、`max_bytes: Option<usize>`（`:175-176`） | 无 |
| 工具 description | `tools.rs:186`：「Read buffered stdout and stderr from a managed background process.」 | `tools.rs:258`：「Kill a managed background process by process_id or name.」 |
| schema 构造 | `process_selector_schema(json!({cursor, max_bytes}))`，`tools.rs:189-202` | `process_selector_schema(json!({}))`，`tools.rs:261-263` |
| 共享 schema 返回 | `tools.rs:320-323`：`{"type":"object","properties":properties}` | 同左 |
| `required` 键 | **不存在**（共享函数 `tools.rs:303-324` 只写 `type` 与 `properties`） | 同左 |
| 顶层 `oneOf/anyOf/allOf/not/enum/const` | 均不存在（`2d441457` 已移除） | 同左 |

`process_selector_schema` 中的两个字段定义（`tools.rs:304-314`）：

```json
"process_id": {
  "type": "string",
  "description": "Managed process id returned by process_start or process_list."
},
"name": {
  "type": "string",
  "description": "Unique managed process name."
}
```

【观察】两个字段的 JSON 类型都是 `string`，但都**没有**出现在任何 `required` 数组中（因为不存在 `required` 键），schema 层面也**没有**"必须且只能二选一"的说明；工具级 description 同样不含 "exactly one"。

### 2.2 运行实例导出的源端 schema

**无法取得。** 原因：Task 1 约束禁止重启/替换实例，也没有提供授权的诊断入口（如 RPC 导出工具定义）来读取运行中实例实际组装的 schema；plan Step 3 明确禁止用仓库生成的 schema 冒充实例导出。

但实例源端 schema 现已可**确定**（无需导出）：实例自报构建 sha `f4a2b433`，而 `git diff f4a2b433..HEAD -- …/process/tools.rs` 为空，即实例编译进去的 `process_selector_schema()` 与 checkout 逐字节相同，产出 `type` + `properties`、无 `required`。这是**源码等价性证明**，不是运行期导出；它足以排除「实例源端把两字段设为必填」这一分支。

---

## 3. 请求端定义

### 3.1 实例实际选用的 provider【观察】

取得途径：只读取 `~/.yi-agent/models.json` 的 provider 相关字段（`api_key` 已脱敏，未记录密钥、URL 凭据、请求头）。

| 字段 | 值 |
|---|---|
| `default_model` | `HORIZON-DeepSeek` |
| `subagent_model` | `HORIZON-DeepSeek` |
| 该模型 `provider` | `openai` |
| 该模型 `api_style` | `chat` |
| `api_url`（主机） | `https://llmapi.horizon.auto/v1` |

【观察】实例默认走 **OpenAI Chat Completions 风格**（`api_style = "chat"`），端点是**仓库外的第三方 OpenAI 兼容网关** `llmapi.horizon.auto`，不是 Anthropic，也不是 Responses。

### 3.2 该风格在仓库内的对应转换点【观察】

| 通道 | 代码位置 | 转换行为 |
|---|---|---|
| Chat Completions | `yi-agent-llm/src/openai/types.rs:118` | `parameters: t.input_schema` —— 逐字透传，不增删键 |
| Responses | `yi-agent-llm/src/openai/responses/types.rs:145` | `"parameters": t.input_schema` —— 逐字透传 |
| Anthropic | `yi-agent-llm/src/anthropic/types.rs:119` | `input_schema: t.input_schema` —— 逐字透传 |
| 注册表 | `yi-agent-core/src/tool.rs:169` | `input_schema: t.schema()` —— 原样取工具 schema |

补充【观察】：

- `git grep -n '"required"' -- 'yi-agent-rs/**/*.rs'` 的全部命中都是**各工具自己显式写出的数组**（如 `fs/read.rs:58`、`process/tools.rs:84`、`shell/bash.rs:102`），**没有**任何一处从"缺失字段"合成 `required`。
- `git grep -n 'properties".*as_object_mut\|input_schema.as_object_mut\|parameters.as_object_mut'` 无命中——仓库内没有任何对工具 schema 的原地改写。
- `yi-agent-relay` crate 中 `git grep -n 'schema'` 无命中——relay 只搬字节，不解读工具定义。
- 仓库内不存在 `"strict"` 字段（`git grep -n '"strict"\|strict:'` 仅命中一处不相关注释）——**strict 模式未启用**。
- `git log -S'"required"' -- yi-agent-llm/src yi-agent-core/src/tool.rs`（`2d441457..HEAD`）为空——provider 层从未引入 required。

### 3.3 不确定性

**故障会话实际使用哪个 provider，不可确定。** 理由【观察】：

- `~/.yi-agent/threads/` 下共 3 个会话，每个是一个 `.jsonl` 加一个 `.meta.json` 边车；元数据只含 `model` / `cwd` / 时间字段；
- 3 个会话的 `.jsonl` 只存 `messages` / `items` / `usage`，**不含工具定义**，且 `grep -l process_read ~/.yi-agent/threads/*` 命中 0 个文件；
- 故障发生时的会话若不在本机落盘范围，则其工具签名没有留存。

因此 §3.1 只能说明"**实例默认配置**走哪个 provider"，**不能**等价于"故障会话走的就是它"。

另有一层**无权限、未取证**：`llmapi.horizon.auto` 这个仓库外网关是否对 `tools[].function.parameters` 做必填化/扁平化/规范化，需要服务端访问权限。当前无此权限，**不可确定**。

---

## 4. 模型可见定义

| 检查项 | 结果 | 性质 / 来源 |
|---|---|---|
| `process_read` 的 `name`、`process_id` 是否标记为可选 | **否 —— 两者都被呈现为非可选** | 用户报告，spec §1 记录为"会话内观察，不是仓库文件或已保存的线上抓包" |
| `process_kill` 同上 | **否 —— 同样两个字段均非可选** | 同上 |
| "必须且只能二选一"的说明是否可见 | **否** —— 源码中工具 description 与字段 description 都没有该说明（§2.1）；实例字符串比对也显示不含该文案（§1） | 观察（源码 + 二进制） |
| 本次取证会话能否直接观察该签名 | **不能** —— 本取证会话的工具集里没有 `process_read` / `process_kill`，无法自证 | 观察 |
| 故障会话是否为新建会话 | **不可确定** —— 故障会话身份未留存，本机 3 个会话的落盘文件中均无 `process_read` 调用 | 观察 |
| 会话刷新是否影响定义 | **不可确定** —— 未取得证据表明工具定义在会话启动时被快照固化；落盘结构只有 messages/items，不含工具定义（§3.3） | 观察（存储结构）+ 未验证 |

---

## 5. 首个变形边界

### 5.1 相邻两层对照

| 层 | `process_id` / `name` 是否必填 | 是否含二选一说明 | 证据 |
|---|---|---|---|
| 源端 schema（实例 `f4a2b433` 与 checkout 逐字节相同） | 都**不是**（无 `required` 键） | 否 | §2.1 `tools.rs:303-324`、`:186`、`:258`；§1 构建 sha 等价性 |
| 注册表 | 原样拷贝，未改 | 否 | `core/src/tool.rs:169` |
| provider 请求（仓库内三个通道） | 原样透传，未改 | 否 | §3.2 四处行号 |
| 仓库外网关 `llmapi.horizon.auto` | **无法取得** | 无法取得 | §3.3 |
| 模型可见定义 | **都被呈现为非可选** | **无** | §4（用户报告） |

### 5.2 判定：**尚未定位**（候选范围已收窄到仓库外）

已排除的两层【观察】：

1. **实例源端**：构建 sha `f4a2b433` 的 `process/tools.rs` 与 checkout 逐字节相同，产出无 `required` 键的 schema（§1、§2）。
2. **仓库内 provider 转发**：四处均为逐字透传，且全仓库不存在 `required` 合成或 schema 原地改写（§3.2）。

另外，`2d441457` 移除的是 `oneOf: [{required:[process_id]}, {required:[name]}]`；即便回退到它之前，schema 也只是"二选一"，**从未**把两个字段同时放进同一个 `required` 数组（`git show 2d441457`）。因此"某个旧版本把两个字段都设为必填"这一假设**在源码层面没有支撑**。

剩下的一环**只可能在仓库之外**，但该层无权限、未取证，故不写成根因：

- **候选 A（主要）**：`llmapi.horizon.auto` 网关在把 Chat Completions `tools[].function.parameters` 转成上游格式（或渲染成给模型的签名）时，把 `properties` 的全部键当作必填——尤其在**整个 `required` 键缺席**时，某些实现会退化为"全部属性必填"。这既解释"两个字段都非可选"，也解释为何源端与转发层都正确。**同时这也是 Task 2 显式写 `required: []` 的真实价值所在**：它与"缺席 required"在标准 JSON Schema 下等价，但对这类把缺席当作全必填的网关不是等价物。
- **候选 B**：故障会话经另一条未在本机留痕的通道（其他客户端/中转），对工具定义另有处置。

**未能排除**：候选 A 与 B 都缺少第一手出站请求体或网关侧配置。**因此边界仍未定位**，只是可选范围从"任意一层"收窄到"仓库外网关或未留痕通道"。

---

## 6. 实施授权

| 授权项 | 状态 | 依据 |
|---|---|---|
| 可修改范围 | 本 worktree 分支内的仓库源码 / 测试 / 文档；即 plan 的 Task 2–4 范围（`yi-agent-tools` 的 schema 与描述、契约测试、provider 请求断言测试） | plan Global Constraints「所有修改在 worktree 分支进行」 |
| 更新/重启运行实例 | **未授权** | plan Global Constraints：「真实实例重启、真实 API 调用、正式 app 替换须分别取得授权」；Task 1 明文禁止重启/替换 |
| 真实 API 调用 | **未授权** | 同上 |
| 仓库外网关的配置或 schema 转换修复 | **无权限、未授权** | 该服务不在本仓库与本机可控范围内 |
| 正式 app 替换 | **未授权** | 同上 |

Task 2–4 可以独立交付源端契约与护栏；**但不得越权宣称 C5/C6 已通过**，也不得据此宣告整体修复完成（spec §7）。

---

## 7. C1–C7 状态

| 编号 | 验收内容 | 状态 | 证据位置 / 阻塞原因 |
|---|---|---|---|
| C1 | schema 合同：object、两选择器均非单独必填、说明含"必须且只能选填一个及省略另一字段"、`required: []` 不破坏兼容性测试 | **通过** | `cargo test -p yi-agent-tools --lib process::tools::tests::process_selector_schema_contract`（Task 2）。先红后绿：红因 `required` 缺失（`left: Null / right: Array []`），不是编译错误。既有 `process_selector_schemas_are_openai_compatible` 同时绿灯，证明 `required: []` 未破坏兼容性 |
| C2 | 参数与运行时：§5.2 输入矩阵，按 name / id 读同一输出，独立实例按 name / id kill | **通过** | `cargo test -p yi-agent-tools --lib process::tools::tests::process_selector_argument_contract`（Task 3）。四种合法 kill 输入各用独立 manager/进程；read 按 name 与 id 读到同一 `selector-ready` 输出 |
| C3 | 拒绝副作用：双字段（含 `""` 占位）与缺席字段的 kill 不改变目标进程状态 | **通过** | 同上：8 组非法输入逐个断言 `status`/`pid`/`ready` 不变且进程仍可读。突变验证：双字段默选其一、空串归一化为 None、kill 拒绝 null 三种突变均被该测试捕获 |
| C4 | provider 边界：wiremock 捕获 Anthropic / Chat / Responses 实际请求，断言两个字符串未都被改为必填 | **通过** | `cargo test -p yi-agent-tools --test process_selector_request_contract`（Task 4）。三条路径各发出一次真实 POST；突变验证：在任一条路径上把 `properties` 全部提升为 required，该测试立即失败（三条路径逐一验证） |
| C5 | 实际公开定义：故障入口 + 新会话中两字段可选、二选一说明可见，保留实例版本与脱敏 schema | **未执行（阻塞）** | (1) 需故障对应的实际入口，但**未授权**重启/更新实例，且**无法取得**实例导出的 schema（§2.2）；(2) 故障会话原始签名**未留存**（§3.3、§4） |
| C6 | 故障入口重验：仅传 name / 仅传 id 可读输出，双有效字段仍互斥报错 | **未执行（阻塞）** | 同上；另需**真实 API 调用**，未授权 |
| C7 | 生效与回归：记录重建/更新/刷新步骤，跑 process 工具兼容性、分页、权限回归 | **部分完成** | 回归已跑：`cargo test -p yi-agent-tools --lib process::`（28 passed，含分页/游标/整组回收/槽位/权限元数据）、`cargo test -p yi-agent-llm`（188 passed）。**生效步骤未验证**：需重建并替换实例二进制、再按 C5/C6 在真实入口确认，两件都**未授权** |

说明：C1–C4 的"通过"只覆盖**仓库内**的源端契约与传输边界。C5/C6 的"未执行"是**授权与入口层面的阻塞**，不是"跑过但失败"；不得把仓库单测绿灯当作 C5 的替代证据（spec §6 明文）。C7 的生效步骤同样未验证。

---

## 8. Step 5 门禁判定

### 判定：(d) 根因修复阻塞

**阻塞原因（按 plan 的四个处理分支逐一交代）：**

| 分支 | 是否成立 | 依据 |
|---|---|---|
| (a) 旧实例版本错误 → 更新源码/版本 | **不成立（已由构建 sha 排除）** | 实例自报 `0.1.13-dev+f4a2b433`，且 `git diff f4a2b433..HEAD -- …/process/tools.rs` 为空，即实例里的 `process_selector_schema()` 与 HEAD 逐字节相同、产出无 `required`（§1、§2）。即便退回 `2d441457` 之前，schema 也只是 `oneOf` 二选一，从不把两字段同时列入 `required`（`git show 2d441457`）。**没有"版本把可选写成必填"的源码证据。** |
| (b) 转换层错误 → 指向已确认文件 | **不成立（无法确认）** | 仓库内四个透传点（`core/src/tool.rs:169`、`openai/types.rs:118`、`openai/responses/types.rs:145`、`anthropic/types.rs:119`）均为逐字透传，无 required 合成；`... as_object_mut` grep 无命中；`yi-agent-relay` 不触碰 schema。**仓库内不存在可指认的变形文件。** |
| (c) 全链路正确 → 检查旧会话定义证据 | **不能宣告** | 模型可见端**确实**呈现为两字段必填（用户报告，spec §1），与"全链路正确"矛盾。且旧会话持有定义**无证据**：本机 3 个会话的落盘文件不存工具定义，也无 `process_read` 调用（§3.3、§4）。 |
| (d) 边界未定位 → 声明阻塞 | **成立** | 缺失的一环只可能位于**仓库外的第三方 OpenAI 兼容网关**（`llmapi.horizon.auto`）或某条未留痕通道，两层均**无权限/无证据**（§5.2、§3.3）。 |

### 由此确定的实施边界

1. **Task 2–4 已完成**（见 §7 C1–C4 通过）：源端契约、运行时护栏、传输护栏均已交付并各自做过突变验证。它们不依赖根因定位（spec §4「定位门禁」允许 §5.1 与 §6 独立完成）。
2. **不得**在本仓库实施任何"猜测性的转换层修改"——没有可指认的文件，改了也无法证明对应真实故障。
3. **整体修复状态保持未完成**。实例源端已由构建 sha 排除，所以解除阻塞只需下列之一：
   - 授权捕获一次真实的 Chat Completions 出站请求体（**只保留** `tools[].function.name/description/parameters`，丢弃 headers 与密钥）。若出站已是"无 `required`、两字段可选"而模型可见端仍为必填，则**根因锁定在网关**；这是最省的一次取证。
   - 取得 `llmapi.horizon.auto` 网关侧的 schema 转换配置或文档，交付对应的外部修复与验收证据。
   - 若网关侧确认"`required` 键缺席 = 全部属性必填"，则 Task 2 的显式 `required: []` 就是对该网关的**实际修复**（而非仅说明改进），但仍须按 C5/C6 在真实入口重验后才能宣告完成。

### Task 5 实际执行结果（仓库外转换边界分支）

Task 5 依据上面的门禁判定只执行了「仓库外转换边界」分支，未实施任何猜测性转换层修改。

**本仓库侧已完成的支撑（Task 2–4）：**

| 项 | 结果 |
|---|---|
| 源端显式契约 | `process_selector_schema()` 现在返回 `required: []`；工具与字段说明写明"exactly one / 省略另一字段 / 不用空字符串占位"（`tools.rs`） |
| 源端合同测试 | `process_selector_schema_contract` 先红（`required` 缺失）后绿 |
| 运行时护栏 | `process_selector_argument_contract` 覆盖 4 种合法单选择器与 8 组非法输入；3 种突变均被捕获 |
| 传输护栏 | `process_selector_request_contract` 覆盖 Anthropic / Chat / Responses；三条路径的"全量 required"突变均被捕获 |

**交付给有权限维护者的脱敏回归步骤**（针对 `llmapi.horizon.auto` 或任何候选网关）：

1. **输入（期望行为）**：`tools[].function.parameters` 为
   ```json
   {"type":"object","properties":{"process_id":{"type":"string"},"name":{"type":"string"},"cursor":{"type":"integer"},"max_bytes":{"type":"integer"}},"required":[]}
   ```
2. **观察点**：该网关实际转发给上游模型（或渲染进提示）的工具签名里，`process_id` 与 `name` 是否仍被标为必填。
3. **期望输出**：两者均为可选，且"必须且只能选一个"的说明可见。
4. **判定**：若入参已是 `required: []` 而出参仍为必填 → 根因锁定在该网关的 schema 规范化；若入参即被改写 → 根因在更上游的客户端/中转。
5. 拿到结论后，把网关侧修复与本文件 §7 的 C5/C6 重验结果一起回填。

**本仓库没有可指认的变形文件，因此没有提交任何"修复转换层"的源码改动**——那会是无法验证的猜测（plan Task 5 Step 1 明文禁止）。

### 对 Task 5 的提示

Task 5 的实际入口验收（C5/C6）在本任务结束时**必然是阻塞状态**。届时若仍未获得上述任一授权，只能记录"未执行（阻塞）+ 所需授权清单"，不得以源码单测或 mock 请求断言代替。

---

## 9. 可复现命令（均不含密钥，只读）

```sh
# 基线
git status --short
git branch --show-current
git rev-parse HEAD
git rev-parse --git-dir
git rev-parse --git-common-dir

# 源端 schema
git grep -n 'process_selector_schema' -- yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs
sed -n '303,324p' yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs
git show 2d441457 -- yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs

# 仓库内是否存在 required 合成 / schema 改写
git grep -n '"required"' -- 'yi-agent-rs/**/*.rs'
git grep -n 'properties".*as_object_mut\|input_schema.as_object_mut\|parameters.as_object_mut' -- 'yi-agent-rs/**/*.rs'
git grep -n '"strict"\|strict:' -- 'yi-agent-rs/**/*.rs'
git grep -n 'schema' -- 'yi-agent-rs/crates/yi-agent-relay/src/*.rs'

# 实例身份（只读）
/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" /Applications/yi-agent.app/Contents/Info.plist
stat -f '%N %Sm' -t '%Y-%m-%d %H:%M:%S' /Applications/yi-agent.app/Contents/MacOS/yi-agent
ps ax -o pid=,lstart=,command= | grep 'MacOS/yi-agent app-server' | grep -v grep
/Applications/yi-agent.app/Contents/MacOS/yi-agent --version   # 自报构建 sha

# 实例源端与 checkout 的逐字节等价（关键证据）
git merge-base --is-ancestor f4a2b433 HEAD && echo ancestor
git diff --stat f4a2b433..HEAD -- yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs   # 应为空
```

---

## 10. 结论摘要

> **本节部分前提已被第 11 节订正。** 运行实例已从 `f4a2b433` 重启为 `308ccaa4`（未修复版本），"实例源码等价于 HEAD"不再成立；候选 A（网关全量提升）已在 11.3 被实验证伪。阅读本节时以第 11 节为准。

- 源端（checkout）与仓库内全部转换层都**保持两个选择器可选**，且**都没有**"二选一"说明。
- 实例自报构建 sha `f4a2b433`（`0.1.13-dev+f4a2b433`），该 sha 处 `process/tools.rs` 与 HEAD 逐字节相同，故实例源端 schema 形状与 checkout 一致（**确定结论**，由源码等价性推出，不需运行期导出）。
- 实例默认 provider 为 **openai / `api_style=chat` / 第三方网关 `llmapi.horizon.auto`**；故障会话实际所用通道**不可确定**。
- **首个变形边界：尚未定位。** 已排除**实例源端**（构建 sha `f4a2b433` 的 `tools.rs` 与 HEAD 逐字节相同）与**仓库内 provider 转换**（逐字透传、无 required 合成）；候选收窄到仓库外网关 `llmapi.horizon.auto` 或未留痕通道。
- C1–C7 全部**未执行**：C1–C4 因测试尚未建立，C5–C7 因实际入口未授权重启、实例 schema 无法导出、真实 API 未授权。
- 门禁判定 **(d) 根因修复阻塞**；Task 2–4 可独立推进，整体修复状态保持未完成。

---

## 11. 第二轮取证（2026-11-03 晚）：实际传输定义 + 网关归一化判别实验

本节订正第 9/10 节的两处状态前提，并补齐 C5 的"实际传输定义"证据。

### 11.1 前提订正：运行实例已换版本

第 9 节记录的实例是 `0.1.13-dev+f4a2b433`。本轮复查时实例已重启，身份变为：

```
ps ax -o pid=,command= | grep 'MacOS/yi-agent app-server'
78611 ... /Applications/yi-agent.app/Contents/MacOS/yi-agent app-server --listen stdio:// --relay ...
/Applications/yi-agent.app/Contents/MacOS/yi-agent --version   ->  yi-agent 0.1.13-dev+308ccaa4
strings -a /Applications/yi-agent.app/Contents/MacOS/yi-agent | grep -c 'Provide exactly one of process_id or name'   -> 0
strings -a /Applications/yi-agent.app/Contents/MacOS/yi-agent | grep -c 'Kill a managed background process by process_id or name'  -> 1
```

`308ccaa4` 是 `main` 当前 HEAD，**不含本分支修复**。因此"运行实例等同于 HEAD 的源码"这一前提不再成立：实例是**未修复**版本。

### 11.2 实际传输定义（C5 的传输层证据）

用一个只记录、不改写的本地 OpenAI 兼容端点（`mock_capture.py`），让**真实二进制**经完整 agent 组装路径发出一次 Chat Completions 请求，只提取 `tools[].function` 的 `name` / `description` / `parameters`（不记录 header、密钥、消息内容）。产物固化为
`docs/research/fixtures/2026-11-03-process-selector-transmitted-definition.json`。

对照结果（同一入口、同一 mock、仅二进制不同）：

| 二进制 | `process_read.parameters.required` | `process_kill.parameters.required` | 二选一说明 |
|---|---|---|---|
| 已部署实例 `308ccaa4`（未修复） | **键缺席** | **键缺席** | 无 |
| 本分支（修复后） | **`[]`** | **`[]`** | 有（描述与两个字段说明） |

结论：修复确实改变了**实际传输的**工具定义，而不只是改了源码 helper（C5 的传输层一半已达成）。剩余一半是"网关之后的模型可见形态"，见 11.3。

### 11.3 判别实验：拒绝"缺失 required 即全必填"这一根因假设

第一轮把"网关把缺席 `required` 当全必填"列为主要候选（§9 候选 A）。本轮对该假设做了可证伪实验：在 mock 端点内实现该归一化规则，再跑同一真实二进制。

| 输入形态 | 归一化后 `process_read.required` |
|---|---|
| 已部署实例（`required` 缺席） | `["cursor","max_bytes","name","process_id"]` |
| 本分支（`required: []`） | `[]`（不被改写） |

**该假设被证伪，理由**：用户报告的是**两个选择器字段**变成必填，而 `process_read` 还有 `cursor`、`max_bytes` 两个属性。全量提升会把它们一并变成必填，与报告不符。第一轮 §9 候选 A 由此**降级为不成立**，第 10 节"候选收窄到仓库外网关"的表述需按此修正：仓库外网关**仍可能**做别的改写，但**不是**全量提升形式。

同时该实验确认了一项真实价值：本分支的 `required: []` 对该类"缺席即全必填"的网关**免疫**（输出保持 `[]`），而已部署版本会退化为全必填。这是显式空列表相对缺席的实际差别所在。

### 11.4 C6 在实际公开入口的执行记录

经**已部署实例**（未修复二进制）的实际 `process_read` / `process_kill` 工具入口执行（工具由运行中的 app-server 提供，非源码单测）：

```
process_start {command: printf selector-c5c6-ready; sleep 180, name: c5c6-probe, ready_pattern: selector-c5c6-ready}
  -> ready=true, process_id=proc_1
process_read {name: "c5c6-probe"}                          -> 成功，stdout=selector-c5c6-ready
process_read {process_id: "proc_1"}                        -> 成功，同输出
process_read {name: "c5c6-probe", process_id: "proc_1"}    -> error: provide exactly one of process_id or name
process_read {name: "c5c6-probe", process_id: ""}          -> error: provide exactly one of process_id or name
process_kill {name: "selector-acceptance-by-name"}         -> killed
process_kill {process_id: "proc_3"}                        -> killed
```

**这是 C6 形态的运行时验收，且是在未修复二进制上通过的。** 但必须明确其判别力边界：运行时校验本来就接受单选择器（`selector_from_args` 只看字段是否存在），所以"name-only 能成功"**不能**证明模型可见定义已修正；它只证明**运行时门不是故障点**，故障只可能发生在模型生成侧（即模型被告知两字段必填，于是总是同时发送两个）。

因此 C6 的严格形态（"不再被入口要求补另一个字符串"）需要**修复版实例**上的新会话才能判定；在未修复实例上执行，只能排除运行时层。

### 11.5 本轮结论

- C5：**传输层达成**（修复版实际传输 `required: []` + 二选一说明，fixture 可复查）；**模型可见层未达成**——需要修复版实例 + 新会话。
- C6：**运行时层已验证**（未修复实例上单选择器可调用、双选择器与空串被拒）；**故障入口层未达成**——同样需要修复版实例。
- 根因：候选 A（全量提升）被证伪；首个变形边界**仍未定位**，但已排除"运行实例源码等价于 HEAD"这一旧前提。
- 待决：是否授权在隔离实例上部署本分支修复以获得 C5/C6 的最终证据（不触碰用户正在使用的实例与真实 API）。

---

## 12. 第三轮：修复版上的完整入口验收（C5/C6 判定）

### 12.1 方法

用一个脚本化的 OpenAI **Responses** 端点替代真实模型：它按固定脚本发出 `function_call`，由**真实 agent 循环**执行**真实进程工具**。因此被验证的是完整链路（工具定义 → provider 请求 → 工具调用 → 运行时校验 → 结果回传），而非源码单测。不接触用户在用的实例，不调用任何真实 LLM API。

隔离环境：`YI_AGENT_HOME=/tmp/c6home`（自带 `models.json`），`--api-url` 指向本地脚本端点，`--api-key dummy-not-a-real-key`。

### 12.2 实际执行的调用序列（修复版二进制）

```
process_start {"command":"printf acceptor-ready; sleep 120","name":"selector-acceptance",
               "ready_pattern":"acceptor-ready","ready_timeout_sec":10}
  -> error=false  {"process_id":"proc_1","name":"selector-acceptance","pid":64844,"ready":true}
process_read  {"name":"selector-acceptance"}                       -> error=false  stdout="acceptor-ready"
process_read  {"process_id":"proc_1"}                              -> error=false  stdout="acceptor-ready"
process_read  {"name":"selector-acceptance","process_id":"not-this-id"}
                                                                   -> error=true   "provide exactly one of process_id or name"
process_kill  {"name":"selector-acceptance"}                       -> error=false  "killed"
```

要点：

- 仅 `name`、仅 `process_id` 各自成功，**都没有被要求补另一个字符串**；`process_id` 用的是本次 `process_start` 实际返回的 `proc_1`，不是历史例子。
- 双字段仍被互斥拒绝（错误文案与修复前一致）。
- kill 的单选择器调用在本次新建的验收实例上进行，未触碰其他托管进程。

### 12.3 C5 / C6 判定

| 项 | 判定 | 依据 |
|---|---|---|
| C5 传输层 | **通过** | 修复版真实传输 `required: []` + 二选一说明（§11.2、fixture） |
| C5 模型可见层（新会话） | **通过（受限于脚本模型）** | 12.2 的新会话里，工具以单选择器成功调用——若定义仍要求双字段，脚本化"模型"不会这样调用；但这验证的是**定义形状**，不是真实模型的遵从行为 |
| C6 单选择器可用 | **通过** | name-only 与 id-only 均成功 |
| C6 双字段互斥 | **通过** | 仍返回 exactly-one 错误 |
| C6 空字符串占位 | **通过** | 未修复实例上已实测被拒（§11.4），修复未触碰该路径 |

### 12.4 仍未达成的部分（必须如实保留）

1. **真实第三方模型的遵从行为未验证。** 脚本端点证明"定义已可选"和"单选择器可调用"，但不能证明真实模型在收到新定义后不会再同时填两个字段。要做这一步必须发起真实 LLM 调用，本轮无此授权。
2. **已部署实例仍是未修复版本。** `308ccaa4` 不含本分支修复，用户当前使用中的入口行为不变。要让它生效需要重新构建并替换实例，未经授权未执行。
3. **首个变形边界仍未定位。** 候选 A（全量提升）已证伪（§11.3）；其余仓库外改写（例如只把"无 required 的 object 里全部 string 属性"提升）无法在无网关访问权限下确认。

### 12.5 结论

C1–C4 与 C5/C6 的**定义层与运行时层**已全部验收；缺口只剩"真实模型遵从行为"与"部署到用户实例"两项，均属需要单独授权的外部动作。据此，功能条目可以从"阻塞"改为"契约已修复且入口已验收，部署与真实模型观察待授权"。
