# 看板时段窗口：在设置界面说明时区口径

日期：2026-10-29
状态：已批准（A1 方案：只做文案说明）

## 问题

桌面「设置 → 插件 → superpowers-kanban」面板里的**时段并发窗口**，每行有星期、
开始、结束、上限，但界面上**没有任何时区信息**。用户看到 `09:00` 无从判断这是
哪个 `09:00`。

判定口径并不在这台设备上。窗口的比较发生在插件进程里：

- `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs`
  的 `ConcurrencyWindow::matches(&self, now: DateTime<Local>)`，用
  `now.hour()` / `now.minute()` / `now.weekday()` 与该窗口的 `start`/`end`/`days`
  比较。
- 调用方是 `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs`
  的 `calendar.limit_at(chrono::Local::now())`。

也就是说窗口是**插件所在那台机器的本地墙钟**。桌面可以经 relay 连远端宿主
（`desktop/src/components/SettingsRemoteTab.tsx`），此时 UI 在本机、墙钟在远端，
用户填的 `09:00` 与实际生效时间不一致，而界面上没有任何线索说明这一点。

配置落盘位置是全局文件
`~/.yi-agent/plugins/superpowers-kanban/superpowers-kanban.toml` 的 `[[window]]`，
字段只有 `days` / `start` / `end` / `all_day` / `max_tasks`
（`calendar.rs` 的 `to_toml` / `settings_json` / `from_settings_json`），
**没有**任何时区字段。

## 目标与非目标

目标：让用户在设置界面就能知道「这些时刻按哪个时区判定」，不必读源码。

非目标（本次明确不做）：

- 不加时区配置项，不改判定语义。
- 不改 `plugin/settings/read` 的返回载荷（不新增字段）。
- 不改 `superpowers-kanban.toml` 的 `[[window]]` 格式。
- 不补 `all_day` 开关（该面板目前只能通过禁用输入间接触及 `all_day`，
  这是独立的既有缺口，不在本次范围）。

## 方案

纯文案，一处 UI + 一处文档。

### 1. 设置面板

文件：`desktop/src/components/SuperpowersKanbanPluginSettings.tsx`

在 `<h3>时段并发窗口</h3>` 之后、窗口列表 `<div>` 之前插入一行说明：

> 以下时刻按运行插件的机器（本地时间）判定，不是本设备的时区；跨时区时两者会不一致。

- 样式沿用文件内已有的 `text-xs text-fg-subtle`，不新增 CSS、不新增依赖。
- **无条件显示**：前端拿不到宿主时区，判不出是否真的跨时区，因此不做条件渲染。
- 纯展示文本：不进入 `KanbanSettings` 形状，不参与 `writeKanbanSettings` 载荷。

### 2. 文档

文件：`plugins/superpowers-kanban/INSTALL.md`，「6.5 并发配额（整机一份）」里的
`[[window]]` 示例之后补一句同口径的说明，避免文档与界面对时间的解释分叉。

## 测试

`desktop/src/components/SuperpowersKanbanPluginSettings.test.tsx` 追加一条用例：
面板渲染完成后，上述说明文案可见。

不新增测试文件；不改既有用例的断言口径。

## 验证

```bash
cd desktop && npx vitest run src/components/SuperpowersKanbanPluginSettings.test.tsx
cd desktop && npx tsc --noEmit
```

两条都通过才算完成。

## 影响面

- 运行时行为：零改动（判定逻辑与落盘格式均不变）。
- 用户可见变化：设置面板多一行说明；INSTALL.md 多一句。
