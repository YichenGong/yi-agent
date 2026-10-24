# MCP 配置自助管理（mcp_reload 工具 + 内置 skill + 设置页开关）

日期：2026-10-25
状态：待实现

## 1. 问题

用户在对话框里说「帮我加一个 GitHub MCP」，当前系统**无法完成这个任务**：

- 唯一入口是手写 `.yi-agent/mcp.json`（Claude Desktop 兼容 schema），需要懂 JSON 结构、
  知道 `stdio` / `http` 两种传输、知道 `args` 怎么排。
- 桌面端「设置 → MCP」面板是**只读**的（`desktop/src/components/SettingsMcpTab.tsx:26`
  「不提供启停开关」），后端只有 `mcp/list`（`yi-agent-app-server/src/rpc/mcp.rs:23`）。
- TUI 的 `/mcp` 只能 `on/off/enable/disable/status`（`yi-agent/src/tui/slash.rs:416`），
  **不能新增**，且 `status` 在未配置时不告诉用户去哪个文件写。
- README 只有一句「通过 MCP 接入外部工具」（`README.md:87`），没有操作说明。

即使手工改对了文件，也**不会立即生效**：`McpManager` 只在启动时由
`register_mcp_tools` → `load_and_probe` 构建一次（`yi-agent-mcp/src/lib.rs:87`），
运行时的 `refresh_registry` 只把内存中的工具重新注册一遍，不重读文件
（`manager.rs:219`）。桌面面板因此写着「重启侧车后生效」
（`SettingsMcpTab.tsx:205`）。

模型侧同样没有指导：内置 skill 只有 `skill-creator` / `skill-installer` /
`git-diff-review`（`yi-agent-skills/src/assets/`），没有任何 skill 讲 MCP 配置怎么写。

## 2. 目标与非目标

**目标**

1. 用户在对话里提出加/关/删 MCP server 时，模型能自助完成，用户不需要懂 JSON。
2. 改完配置当轮不生效，但**下一轮一定生效**，不需要重启进程。
3. 每一条失败都以原文回报给模型，模型能据此自行改正。
4. 配置写入受权限确认保护，用户看得懂将执行什么。
5. 桌面设置页有与「开」对称的「关」。

**非目标（本期不做）**

- 用户级 `~/.yi-agent/mcp.json`（第三层作用域）。当前配置只有项目级一层：
  `config_path(workdir) = <workdir>/.yi-agent/mcp.json`（`config.rs:147`）。
- SSE 传输（只支持 `stdio` / `http`，见 `ServerKind`，`config.rs:20`）。
- 文件监听（`notify`）自动重载。
- 同一轮内新增工具（见 §4.1 的硬约束）。
- MCP server 的图形化编辑已有条目的全部字段（设置页只做启停）。

## 3. 关键约束（已核实）

**3.1 同一轮内无法新增工具。** `Agent` 持有 `Arc<ToolRegistry>`
（`yi-agent-core/src/agent.rs:236`），`ToolRegistry` 内部是不可变 `BTreeMap`
（`tool.rs:135-137`），`run_loop` 在回合开始时 clone 该 Arc（`agent.rs:738`）。
工具在自己执行期间**不能**往本轮工具表里加工具。因此「生效」只能发生在回合之间，
由外部重建完成。

**3.2 工具结果必须诚实。** 既然不能当轮生效，工具返回文案必须写「下一条消息起可用」。
若谎称「已生效」，模型下一步会调用不存在的工具并拿到 unknown tool，白白浪费一步。

**3.3 探测是唯一的信息来源，且可能很慢。** MCP 工具的名字、描述、`input_schema`
只能问 server 才有（`manager.rs:438` 的 `probe_tools`：connect 30s + list 20s +
reap 5s，最坏 55s/server，且 `load_and_probe` 顺序探测）。
结果缓存在 `.yi-agent/mcp-cache.json`，用 `ServerConfig` 的 SHA-256 指纹失效
（`cache.rs:52`）。

**3.4 换掉 manager 会留下孤儿工具。** `refresh_registry` 与 `all_tool_names` 都读
`self.servers`（`manager.rs:182-226`）。被删除的 server 名字已不在其中，
旧 `mcp__*` 工具会永久残留在 registry 里。

**3.5 权限模型默认放行未知工具。** `PermissionChecker::check` 只对
`bash | process_start | process_kill | write | edit` 做检查，其余一律
`CheckResult::Allow`（`permission.rs:131-137`）。`mcp_reload` 若不加进去，
会被**沉默放行**——而它能让模型写入一条下次启动自动执行的 `command`。

## 4. 设计

### 4.1 生效模型：回合之间重建

用户感知的完整链路：

```
用户：帮我加一个 GitHub MCP
  → 模型加载 mcp-setup skill，知道该调 mcp_reload
  → mcp_reload{action:"add", name:"github", command:"npx", args:[...]}
  → 权限确认（界面显示：将执行 npx -y @modelcontextprotocol/server-github）→ AllowOnce
  → 校验 → 合并写入 .yi-agent/mcp.json → 只探测 github
  → 返回："github ✓ 7 个工具（4 只读）。新工具从下一条消息起可用。"
  → 下一轮开跑前：driver 检测到配置指纹变化 → reload + 重建 agent
  → 模型此时可调用 mcp__github__*
```

**检测点（两端各一处，均为廉价操作）**：读完 `mcp.json` 算指纹并与 manager 上次
记录的比较，未变则跳过。位置与现有 skills catalog 刷新同处——即每轮 prompt 到来、
`agent.run(...)` 之前：
- TUI：`yi-agent/src/main.rs:2353`（`DriverInput::Prompt` 分支内，`agent.run` 于 `:2358`）
- 桌面：`yi-agent-app-server/src/server.rs:5160`（每轮开跑前刷新 catalog 的同一处，
  `agent.run_blocks` 于 `:5166`）

注意 TUI 侧选 `:2353` 而不是 `:1997`：后者是 `DriverControl` 控制命令分支，只在
收到控制命令时执行；prompt 轮次走的是 `:2321` 之后的 prompt 分支。放在控制分支里
会导致「本会话从未发过控制命令」时重载永不触发。

这条检测顺带修掉「手改 `mcp.json` 必须重启」的旧缺口。

**兜底入口**：TUI `/mcp reload`（走 `DriverControl` 分支，立即强制刷新一次）、
桌面 `mcp/set`（同时承担启停）。

### 4.2 `mcp_reload` 工具（一个工具，`action` 多动作）

放在 `yi-agent-mcp` crate（不能放 `yi-agent-tools`：后者只依赖 `core` + `skills`，
加进来会反向依赖，破坏现有分层）。

```json
{
  "name": "mcp_reload",
  "description": "添加、启用、停用或删除一个 MCP server。改动写入 <workdir>/.yi-agent/mcp.json，从下一条消息起生效。",
  "parameters": {
    "type": "object",
    "properties": {
      "action":  { "type": "string", "enum": ["add", "disable", "enable", "remove"] },
      "name":    { "type": "string", "description": "server 名，小写字母/数字/连字符，<=64 字节" },
      "kind":    { "type": "string", "enum": ["stdio", "http"], "description": "仅 add 用，默认 stdio" },
      "command": { "type": "string", "description": "仅 stdio add 用" },
      "args":    { "type": "array", "items": { "type": "string" } },
      "env":     { "type": "object", "additionalProperties": { "type": "string" } },
      "url":     { "type": "string", "description": "仅 http add 用" },
      "headers": { "type": "object", "additionalProperties": { "type": "string" } },
      "overwrite": { "type": "boolean", "description": "同名 server 已存在时是否覆盖，默认 false" }
    },
    "required": ["action", "name"]
  }
}
```

各动作语义：

| action | 改动 | 是否探测 |
| --- | --- | --- |
| `add` | 写入 `mcpServers[name]`；同名已存在且未传 `overwrite: true` 则拒绝 | 是，只探这一个 |
| `disable` | 设 `mcpServers[name].enabled = false` | 否，秒回 |
| `enable` | 设 `mcpServers[name].enabled = true`；与 `disable` 对称，不重探（已有缓存或本轮曾探测过） | 否，秒回 |
| `remove` | 从 `mcpServers` 删除该条 | 否 |

**注册与配置解耦**：`register_mcp_tools` 当前在无 `mcp.json` 时返回 `Ok(None)` 且
不注册任何工具（`lib.rs:87-91`）。而「首次添加」恰恰是文件不存在的状态，
因此 `mcp_reload` 必须**无条件注册**，独立于 `mcp.json` 是否存在。

### 4.3 `McpManager` 支持重载

- `servers` 由构造后不可变改为 `RwLock<HashMap<String, ServerEntry>>`。
  受影响读取点共 9 处（`manager.rs:135/152/161/183/196/239/287/331/349`），
  均为短临界区，不跨 `.await` 持锁。
- 新增 `reload(&self, workdir) -> ReloadReport`：
  1. 重读 `mcp.json`（缺失视为空配置）
  2. 逐 server 比较指纹：未变化的沿用内存中的工具清单，**不重探**
  3. 探测新增/变化的 server（沿用 `probe_tools` 超时语义）
  4. 原地替换 `servers`；仍存在的 server 保留其会话内开关状态
  5. 更新 `last_registered` 并**先 `remove` 掉不再存在的工具名**
- 新增 `last_registered: Mutex<HashSet<String>>`，修复 §3.4 的孤儿工具问题：
  `refresh_registry` 改为按 `last_registered` 逐个移除，而不是按当前 `servers` 推导。

### 4.4 校验规则（写盘前，任一失败即返回且不写盘）

| 校验 | 规则 | 失败消息（用户可读） |
| --- | --- | --- |
| name | `^[a-z0-9]([a-z0-9-]*[a-z0-9])?$`，<=64 字节 | name 只能用小写字母/数字/连字符，且不能首尾为连字符 |
| kind | 缺省 `stdio`；只接受 `stdio` / `http` | 不支持的 type: "sse"，只支持 stdio / http |
| stdio 必填 | `command` 非空 | stdio 需要非空的 command |
| http 必填 | `url` 以 `http://` / `https://` 开头 | http 需要合法的 url |
| 重名 | 同名已存在且未传 `overwrite` | server "github" 已存在；要覆盖请传 overwrite: true |
| env / headers 变量 | 值含 `${VAR}` 且 `VAR` 未定义 → 拒绝 | env.GITHUB_TOKEN 引用的环境变量 GITHUB_TOKEN 未定义 |
| header 名 | `^[A-Za-z0-9-]+$` | header 名不合法（只允许字母/数字/连字符） |

`${VAR}` 那条是对现有行为的有意收紧：运行时 `expand_headers` 会静默丢弃该 header
并只打 warn（`config.rs:127-135`），用户最终面对 401 却查不到原因。改成写入时
直接拒绝，把问题暴露在当下。

name 规则与 `naming.rs` 的字符清洗对齐，避免两个不同名字被清洗成同一工具名。

### 4.5 危险命令拦截

`mcp_reload` 的本质是让模型在用户机器上启动任意子进程（stdio 型）。

- **复用现有黑名单**：`BlocklistFn`（`permission.rs:82`）由调用方注入给
  `McpReloadTool`，与 `PermissionChecker` 已有的注入方式一致。不把
  `yi-agent-tools` 的 `blocklist` 下沉到 core，避免 crate 重构。
- **本地规则**：`command` 含 `/` 时必须是存在的绝对路径（防 `./evil.sh`）；
  不含 `/` 时按裸可执行名处理。

### 4.6 权限确认（不受 yolo 豁免）

在 `permission.rs:131` 的受控工具列表加入 `mcp_reload`，并让它**始终**
`NeedConfirm`，判断放在 `yolo.get()` **之前**：

```rust
if tool_name == "mcp_reload" {
    return CheckResult::NeedConfirm(
        self.build_request(tool_name, tool_input, PermissionKind::Normal),
    );
}
```

`prefix_suggestion` 保持 `None`（`build_request` 只为 bash / process_start 生成，
`permission.rs:248`），即**不支持 AlwaysAllowPrefix**——避免变成永久静默口子。

确认界面渲染一行人类可读摘要即可，例如：
`添加 MCP server "github"，将执行：npx -y @modelcontextprotocol/server-github`
（`disable` / `enable` / `remove` 相应渲染为「将停用/启用/删除 …」，不涉及命令执行）。

### 4.7 设置页开关（桌面端）

- 后端新增 `mcp/set`：`{ project?, scope: "master" | "server", name?, enabled }`，
  需 `Control` 档（照 `rpc/flags.rs:23-45` 的门禁写法）。写盘后请求重建。
- 前端 `SettingsMcpTab.tsx`：徽章换开关，调用 `mcp/set`；`SettingsFlagsTab.tsx:40`
  是现成范式。补 `desktop/src/lib/mcp.test.ts`。
- 文案修正：`SettingsMcpTab.tsx:205` 的「重启侧车后生效」改为「下一轮对话生效」。

### 4.8 内置 skill：`mcp-setup`

`yi-agent-skills/src/assets/mcp-setup/SKILL.md`，随应用安装到
`~/.yi-agent/skills/.system/mcp-setup/`（`system.rs:6` 的 `include_dir`）。

frontmatter：`name: mcp-setup`，`description` 需覆盖「加/关/删 MCP server、
排查 MCP 配置问题」这些触发场景，否则模型不会加载它。

正文只讲模型需要的判断，不抄 API 文档：
1. 何时用 `mcp_reload`（vs 直接 `write` 改文件）
2. `action` 各值怎么选、`stdio` 与 `http` 如何分辨
3. 失败结果怎么读、怎么改（超时 → 多半是 `npx` 拉包慢或包名错；401 → header 变量）
4. 生效时机（下一轮），不要在本轮尝试调用新工具
5. 删除/大改已有条目的替代路径

## 5. 错误处理

**绝不吞错误**。`load_and_probe` 现在的做法是 `warn!` 后留空
（`manager.rs:92-97`），重载路径必须改为收集 `Result` 并原样回报：

```
github  ✓ 7 个工具（4 只读）
fs      ✗ MCP connect timed out after 30s
```

其它边界：

| 情况 | 行为 |
| --- | --- |
| `mcp.json` 不存在 | 正常创建（首次添加），不算错误 |
| 文件是坏 JSON | 拒绝写入，回报 `serde_json` 自带的行号/列号 |
| 写盘 | 临时文件 + rename，避免半写状态被下一轮读到 |
| 探测超时 | 配置已保存仍然成功，返回中明确区分「配置已保存」与「探测失败」 |
| 只读沙箱 / 无写权限 | 返回明确错误，不静默降级 |

## 6. 测试

**yi-agent-mcp**
- §4.4 校验表逐行单测
- 指纹未变不重探：用 `AtomicUsize` 计数探测次数（仿 `manager.rs:850` 既有写法）
- `remove` 后旧工具名从 registry 消失（钉住 §3.4）
- 坏 JSON 回报行号
- `${VAR}` 未定义被拒
- `reload` 保留仍存在 server 的会话开关状态

**yi-agent-core**
- `mcp_reload` 在 yolo=true 下仍 `NeedConfirm`
- yolo=false 下同样 `NeedConfirm`
- 不产生 `AlwaysAllowPrefix`

**yi-agent（TUI）**
- 改 `mcp.json` 后下一轮 registry 出现新工具名
- `/mcp reload` 立即生效

**yi-agent-app-server**
- `mcp/set` 在 Observe 档被拒（`insufficient_scope`）
- `mcp/set` 写盘后下一轮 registry 出现/消失对应工具

**yi-agent-skills**
- `installs_to_empty_dir` 覆盖新 asset 存在性（现有测试会自动覆盖）

**desktop**
- `SettingsMcpTab.test.tsx`：开关调用 `mcp/set` 且参数正确
- `npx vitest run` + `npx tsc --noEmit`

## 7. 分期

**本期（全部）**

1. `McpManager::reload` + `last_registered` 清理（含 §3.4 修复）
2. `mcp_reload` 工具（action 多动作）+ 校验 + 黑名单注入
3. 权限接线（始终 NeedConfirm）
4. 两端每轮指纹检测 + 重建
5. `/mcp reload` + `mcp/set` + 面板开关 + 文案修正
6. 内置 `mcp-setup` skill
7. 文档同步：`docs/project-management/yi-agent-mcp.md`、`yi-agent-skills.md`、
   `desktop.md`，README 增加「接入 MCP」小节

**后续（不在本期）**

- 用户级配置 `~/.yi-agent/mcp.json` 与三层合并优先级
- 文件监听自动重载
- 设置页图形化编辑 server 全字段
- 同轮生效（需改 `ToolRegistry` 为 `RwLock`，属 core 公共 API 变更）

## 8. 风险

| 风险 | 缓解 |
| --- | --- |
| `RwLock` 化波及 `call_tool` 热路径 | 所有临界区短、不跨 `.await`；读数处先取快照再放开锁 |
| 探测慢（首次 `npx` 拉包） | 只探变化项；工具返回逐 server 结果；TUI 有 spinner + elapsed（`tui/state.rs:37`），不是黑屏 |
| 模型误判「已生效」而调用新工具 | 返回文案强制包含「下一条消息起可用」；skill 正文重申 |
| 权限确认造成摩擦 | 仅一次；`disable` / `enable` / `remove` 不涉及命令执行，但同样确认（都在改可执行配置） |
| 旧工具残留（§3.4） | `last_registered` 显式清理 + 专门单测 |
