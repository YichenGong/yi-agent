# `/help` 分组卡片设计

## 状态

设计已由用户确认。

## 背景

桌面端的 `/help` 目前走 `session.notice()` 把一段多行纯文本塞进会话流
（`desktop/src/App.tsx:1621` 调用 `renderHelp()`）。`ChatView` 把 notice 渲染成
一个居中的小灰胶囊（`desktop/src/components/ChatView.tsx:163`）：

```
可用命令:
  /clear 清空对话上下文
  /compact 压缩对话历史
  /config 显示当前配置
  ...
```

三行以上就撑破胶囊，等宽对齐全靠空格，扫读性差。同一份命令目录在输入框上方
的 `SlashPopup` 里已经是「等宽命令 + 灰色说明」的两列结构，但 `/help` 没跟上。

对照组：`/config` 已经卡片化（`desktop/src/components/ConfigCard.tsx`），走的是
`Session.noticeConfig` 携带结构化载荷、`ChatView` 按载荷分派渲染的路径。`/help`
是同一类"命令输出"，理应享受同等待遇。

## 目标

1. `/help` 输出一张卡片：标题 + 分组 + 每行「等宽命令 + 说明」，视觉语言与
   `ConfigCard` 一致。
2. `/help <command>` 输出该命令的详情卡（用法 / 说明）。
3. `/help <unknown>` 与未知命令 `/nope` 输出同一张卡：顶部一行「未知命令」，
   下面照常给完整分组清单。
4. 命令清单仍以 `desktop/src/lib/slash.ts` 的 `SLASH_COMMANDS` 为唯一事实源，
   不复制第二份。
5. 不触碰 agent：命令输出永远只是本地 notice。

## 非目标

- 不做可点击回填输入框的命令列表（新交互，超出「看得清楚」的范围）。
- 不覆盖斜杠命令以外的功能（快捷键、面板、附件等）。
- 不改 `SlashPopup` 的外观与过滤行为。
- 不改服务端 notice（`thread/notice`）的渲染。
- 不改 TUI（`yi-agent-rs/crates/yi-agent/src/tui/slash.rs`）的输出。

## 方案

### 数据：分组表与命令目录同源

在 `desktop/src/lib/slash.ts` 紧挨 `SLASH_COMMANDS` 增加：

```ts
export const HELP_GROUPS: { title: string; commands: string[] }[] = [
  { title: "会话", commands: ["clear", "compact"] },
  { title: "状态", commands: ["config", "cost", "model"] },
  { title: "帮助", commands: ["help"] },
];
```

分组只影响呈现，**不改变 `SLASH_COMMANDS` 的顺序**（Popup 的顺序由现有测试钉死）。

一致性由测试保证：把 `HELP_GROUPS` 拍平后必须恰好等于 `SLASH_COMMANDS` 的名字
序列（同集合、无重复、无遗漏）。新增命令却忘了分组时测试直接失败。

### 模型：notice 载荷改成判别联合

目前 `NoticeItem` 有一个可选的 `config` 字段（`desktop/src/lib/session.ts:46`）。
再加一个并列的 `help?` 会允许"两个都填"这种无意义状态，因此改成判别联合：

```ts
export type NoticeBody =
  | { kind: "config"; data: Record<string, unknown> | null; error: string | null }
  | { kind: "help"; target: string | null; error: string | null };

export interface NoticeItem {
  type: "notice";
  id: string;
  text: string;
  level?: "Info" | "Warn";
  body?: NoticeBody;
}
```

`Session.noticeConfig(data, error)` 替换为 `Session.noticeBody(body: NoticeBody)`；
`/config` 的调用点改为传对象，行为与外观完全不变。`text` 在带 `body` 时仍为空串
——卡片就是呈现，没有其他消费者读它。

### 渲染：HelpCard

新增 `desktop/src/components/HelpCard.tsx`。外壳类名逐字复用 `ConfigCard`：

```
my-2 self-stretch rounded-lg border border-line-strong bg-surface px-3 py-2 text-sm
```

三种形态：

| 输入 | 标题 | 内容 |
| --- | --- | --- |
| `/help` | `可用命令` | 每个分组一个小标题（`text-xs text-fg-subtle`）+ 命令行 |
| `/help cost` | `命令 /cost` | `用法` / `说明` 两行，标签列宽 `w-24` |
| `/help nope`、`/nope` | `可用命令` | 顶部红字 `未知命令: /nope` + 完整分组清单 |

命令行结构：`w-40 shrink-0 font-mono text-fg` 的命令（含 `usage`）+ `text-fg-muted`
的说明；说明允许收缩换行，命令列不换行。命令列的 `w-40` 是按现有最长项
`/compact`、`/config`、`/model` 与 `/help [command]` 估算的，比 `ConfigCard` 的
`w-24` 宽，避免等宽命令被折断。

未知目标的红字用与 `ConfigCard` 错误行相同的 `text-red-300`。

`ChatView` 的分派点（`desktop/src/components/ChatView.tsx:163`）从"看 `config`
字段"改为"看 `body.kind`"：`config` → `ConfigCard`，`help` → `HelpCard`，
无 `body` → 现有灰色胶囊。服务端 `thread/notice` 不带 `body`，保持原样。

### 移除 renderHelp

`/help`、`/help <cmd>`、未知命令三条路径全部卡片化后，`renderHelp` 没有调用者，
删除，其测试断言迁入 `HelpCard.test.tsx`。`App` 的 `default` 分支改为
`session.noticeBody({ kind: "help", target: null, error: \`未知命令: /${name}\` })`，
让拼错的命令与 `/help nope` 落到同一张卡上。

### 测试

- `lib/slash.test.ts`：分组拍平 == 命令目录；三组标题存在。
- `components/HelpCard.test.tsx`：分组与全部命令可见；详情卡显示用法与说明；
  未知目标显示红字且仍有清单；卡片带 `data-testid="help-card"`。
- `App.test.tsx`：`/help` 出 `help-card` 且不发 `turn/start`；`/help cost` 显示用法；
  `/nope` 显示未知命令；`/config` 仍出 `config-card`（回归）。
- `lib/session.test.ts`：`noticeBody` 携带 help 载荷，`text` 为空串。

## 影响面

- 改动：`desktop/src/lib/slash.ts`、`desktop/src/lib/session.ts`、`App.tsx`、
  `ChatView.tsx`，新增 `HelpCard.tsx`。
- 删除：`renderHelp` 及其测试。
- 不涉及 Rust、线协议、服务端。
