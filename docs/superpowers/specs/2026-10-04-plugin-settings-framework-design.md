# 插件设置框架（Plugins tab）设计

日期：2026-10-04
状态：待评审

## 1. 背景与问题

看板的并发与时间配置今天只能改文件，没有界面：

- 并发日历在 `<项目>/.yi-agent/superpowers-kanban/superpowers-kanban.toml`，
  由 `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs`
  的 `ConcurrencyCalendar` 读取（`default_max_tasks` + 若干 `[[window]]`，
  每个窗口含 `days` / `start` / `end` / `max_tasks`）。界面无从编辑。
- 推进间隔（runner 主循环的睡眠周期）写死在 supervisor 清单
  `supervisors/superpowers-kanban.json` 的 `--interval-secs 10` 里，改它要动清单。

用户需求：

1. 设置里有一个 **Plugins tab**，里面有 superpowers-kanban，能设置对应的东西。
2. 至少要有**并发数量**和**时间**相关设置。
3. 架构上**必须装了插件才有对应设置界面**；非插件中不应出现相关内容。

第 3 条是本设计的主约束：通用壳子（设置、宿主、协议）里不得含任何看板语义，
看板设置只能作为「一个插件的设置」存在，且随插件安装而出现。

## 2. 决策记录（本次头脑风暴敲定）

| 问题 | 决定 |
| --- | --- |
| 通用程度 | **A：通用插件框架**。宿主枚举已装插件，桌面端按名字注册面板。superpowers-kanban 是第一个。 |
| 「已安装」范围 | **A：按 app-server 自身 workdir** 的 `<workdir>/.yi-agent/supervisors/*.json`。 |
| 覆盖端 | **全栈**：宿主 RPC + 桌面端 + TUI。 |
| 面板内容 | **并发窗口编辑器 + 推进间隔秒数**（完整窗口表，非精简版）。 |
| 推进间隔归属 | **迁入插件自己的配置** `superpowers-kanban.toml`，runner 每 tick 重读，改完下个 tick 生效、无需重启。 |
| Plugins tab 可见性 | **常驻**；无插件时中性空状态「未安装任何插件」。 |
| 名字 → 面板映射 | **桌面端名字注册表 + 未知插件中性兜底**（未注册也列出，显示「该插件暂无可配置项」，不静默消失）。 |
| TUI 窗口编辑 | **只读**：TUI 能列插件、看设置、改标量（`default_max_tasks` / `interval_secs`），窗口表只展示并提示去桌面编辑。 |

## 3. 目标与非目标

**目标**

- 设置里新增常驻 Plugins tab；列出本机（app-server workdir）已装插件。
- 每个插件可提供自己的设置面板；superpowers-kanban 提供「并发 + 时间」面板。
- 宿主新增一套**通用**插件通道（枚举 + 设置读写），宿主只转发、不解释语义。
- 没装插件 → 不出现该插件任何设置内容。

**非目标**

- 不做完全 schema 驱动的通用表单（表达不了「星期/时段窗口」这种复合编辑器）。
- 不做宿主级全局插件目录（沿用 per-project supervisor 布局）。
- 不动现有 `plugin/query`（看板专用通道）及其行为。
- 桌面端不做插件设置的乐观更新（写失败必须内联报错，不改显示值）。

## 4. 架构

三层，宿主居中只做转发：

```
桌面端 / TUI
   │  plugins/list               ← 枚举 <workdir>/.yi-agent/supervisors/*.json（纯文件，不依赖 daemon）
   │  plugin/settings/read       ┐ 宿主只转发，不解释语义
   │  plugin/settings/write      ┘
   ▼
app-server（通用插件通道，作用域 = 自身 workdir）
   │  IpcRequest::PluginQuery  →  <workdir>/.yi-agent/runtime/daemon.sock
   ▼
项目 daemon → 插件 socket（superpowers-kanban 的 query_socket）
   │  settings.read / settings.write
   ▼
插件自有配置 <workdir>/.yi-agent/superpowers-kanban/superpowers-kanban.toml
```

要点：

- 新通道作用域是 **app-server 自己的 workdir**，不带 `project` 参数，**不受**
  看板登记表（`registry::contains`）门控——那是看板专用通道 `plugin/query` 的约束。
- 插件清单来自 `yi_agent_supervisors::manifest::load_manifests(dir)`（公开函数，
  `yi-agent-supervisors/src/manifest.rs:183`），目录 = `Layout::for_workdir` 的
  `manifests_dir()`（`supervisor.rs:28`）。枚举已装插件是纯文件读取，daemon 没起也能列。
- 现有 `plugin/query` / `board_query`（server.rs:984、1062）**保持原样并存**。

## 5. 宿主侧 RPC（app-server）

### 5.1 `plugins/list`

无需参数。返回：

```json
{ "plugins": [
  { "name": "superpowers-kanban", "queryable": true, "switch_key": "superpowers_kanban" }
] }
```

- 数据源：`load_manifests(<workdir>/.yi-agent/supervisors)`。
- `queryable` = 该清单声明了 `query_socket`（即插件提供设置/查询通道）。
- 目录缺失 → 空列表，不是错误。
- 已装（清单在）但未运行时，`plugins/list` 依然列出它——「装了」看清单，
  「在跑」看后续读写是否得到 `plugin_unavailable`。

### 5.2 `plugin/settings/read`

参数 `{ "plugin": "<name>" }`。宿主编 `IpcRequest::PluginQuery { plugin, method: "settings.read", params: {} }`，
发往 `<workdir>/.yi-agent/runtime/daemon.sock`，把插件应答的 `settings` 原样返回。

### 5.3 `plugin/settings/write`

参数 `{ "plugin": "<name>", "settings": <object> }`。转发 `settings.write`，
成功返回 `{ "ok": true }`；插件校验失败则结构化错误透传。

### 5.4 错误码

| 情形 | code | 含义 |
| --- | --- | --- |
| 清单里没有该插件 | `plugin_not_installed` | 没装；前端不显示该插件任何内容 |
| 清单有、插件没跑 / 无 socket | `plugin_unavailable` | 装了但没运行；控件禁用并提示 |
| daemon 不可达 | `plugin_unavailable` | 同上处理（复用既有措辞） |

沿用 `protocol.rs` 既有的 `plugin_unavailable` 码位（-32022）。新增 `plugin_not_installed`：
它是**协议 vocabulary 里的新 `data.code`**，落在一个新的数字码上（如 `-32023`），
与既有 `board_query` 的「数字码只是粗回退、`data.code` 才是稳定词表」一致
（`protocol.rs:136-148`）；前端只读 `data.code`。

## 6. 插件侧（superpowers-kanban）

### 6.1 新增分派方法（`crates/superpowers-kanban-runner/src/dispatch.rs`）

- `settings.read` → `{ "settings": { "default_max_tasks": N, "interval_secs": N, "windows": [ ... ] } }`。
- `settings.write`（`params.settings`，**全量替换**）：**先校验后落盘**，非法则报错且
  **一个字节不写**（与 `enqueue` 的「拒绝即零落盘」同一约定）。

校验规则：

- `default_max_tasks`、每个 `window.max_tasks`：正整数（≥ 1）。
- `interval_secs`：整数，范围 `[1, 3600]`。
- 窗口 `days`：合法星期简写；`start`/`end`：`HH:MM`（`24:00` 允许，归一为当日末尾），
  起 < 止（除 `24:00` 情形），`all_day` 为真时忽略起止。
- `windows` 顺序即匹配优先级。

### 6.2 推进间隔迁入插件配置

- `superpowers-kanban.toml` 新增 `interval_secs`（缺省 10）。
- runner 主循环改成**每 tick 重读**该配置取间隔；写设置后**下个 tick 生效，无需重启**。
- 优先级：显式 CLI `--interval-secs` > 配置文件 > 默认 10（CLI 参数保留，测试与旧清单兼容）。
- supervisor 清单去掉 `--interval-secs 10`（改为由插件配置决定）。

### 6.3 core 侧（`superpowers-kanban-core`）

- `ConcurrencyCalendar` 补 `interval_secs` 字段、序列化（`to_toml`）与落盘（原子写）。
- 写只写新名 `superpowers-kanban.toml`；旧 `kanban.toml` 仍只读兼容（沿用
  `load_preferring_new`），不修改、不删除。

## 7. 桌面端

### 7.1 SettingsDialog

- `TABS` 增加第三项 `{ id: "plugins", label: "插件" }`（`SettingsDialog.tsx:8-11`）。
- Plugins 面板常驻；无已装插件时显示中性空状态「未安装任何插件」。

### 7.2 `SettingsPluginsTab.tsx`（新增）

- 载入时调 `plugins/list`。
- 逐个插件按**名字注册表**选面板组件；注册表当前仅一项：
  `"superpowers-kanban" → SuperpowersKanbanPluginSettings`。
- 注册表未命中的插件仍列出，显示「该插件暂无可配置项」（中性兜底，不静默消失）。
- 插件装了但读设置得到 `plugin_unavailable` → 面板显示「插件未运行」、控件禁用。

### 7.3 `SuperpowersKanbanPluginSettings.tsx`（新增）

- 字段：默认并发上限、窗口表（每行：星期 / 起 / 止 / 上限，可增删改）、推进间隔秒数。
- 读写走 `plugin/settings/{read,write}`；写成功后以前端重读的结果为准，**不做乐观更新**。
- 写失败内联报错，保留用户输入。

### 7.4 现有看板面板

`App.tsx:1229` 的 `SuperpowersKanbanSettings`（看板开关 + 后台值守）**保留原位**——
那是看板视图的控制，不是插件设置，不在 Plugins tab 重复。

### 7.5 协议封装

新增 `desktop/src/lib/pluginSettings.ts`：`listPlugins` / `readPluginSettings` /
`writePluginSettings`，以及把错误映射到 `not_installed` / `not_running` / `other`
的纯函数（与 `boardIndex.ts` 的风格一致，可单测）。

## 8. TUI

- `/plugins`：列已装插件（`plugins/list`）。
- `/plugins <name>`：打印该插件当前设置（`settings.read`，窗口表只读展示）。
- `/plugins <name> set default_max_tasks N`、`set interval N`：写标量。
- 窗口表编辑提示去桌面端操作（v1 TUI 不编辑窗口表）。

## 9. 错误处理与门控

- **未装**（清单无该插件）：Plugins tab 不渲染该插件任何内容。
- **装了但没跑**：条目在，面板显示「插件未运行」、控件禁用。
- **写失败**：内联报错，不改前端显示值。
- 插件返回畸形 JSON：当作「暂无可配置项」或读失败，不 panic。

## 10. 测试

**Rust（宿主）**

- `plugins/list`：枚举多个清单；无 socket 的插件 `queryable=false`；目录缺失 → 空。
- `plugin/settings/read|write`：转发到 daemon（沿用既有 `plugin_query_tests` 的
  socket 桩法）；清单无该插件 → `plugin_not_installed`。

**Rust（插件）**

- `settings.write` 校验：非法 interval / 非法时间 / 非正并发 → 报错且**零落盘**。
- `settings.read` 往返：写完读回一致。
- runner 每 tick 重读 interval：改文件后下个 tick 使用新值。

**桌面端**

- `SettingsDialog`：第三个 tab 存在、可切、aria 关系正确。
- `SettingsPluginsTab`：空状态；注册表命中渲染面板；未命中渲染兜底条目。
- `SuperpowersKanbanPluginSettings`：读写调用正确、读到 `plugin_unavailable` 时禁用。
- `pluginSettings.ts` 纯函数错误映射。

**TUI**

- `/plugins` 列出与 `set` 的最小用例。

## 11. 迁移与兼容

- 旧清单带 `--interval-secs` 仍可用（CLI 优先）。
- 旧 `kanban.toml` 仍只读兼容；写只写 `superpowers-kanban.toml`。
- INSTALL.md / README 更新：删掉过时的「默认 60 秒」，说明推进间隔现在在插件设置里改。
- `docs/project-management/desktop.md` 与 `yi-agent-app-server.md` 登记新 RPC 与新设置项。

## 12. 未决/风险

- 插件设置面板在桌面端是 TSX（非 schema 驱动）——已知取舍，见 §2。新增插件需在注册表加一行。
- 「已安装」按 app-server workdir，若 app 与该项目的 daemon 不同源，插件设置可能读不到——
  与现有看板通道同源问题一致，本次不扩大范围。
