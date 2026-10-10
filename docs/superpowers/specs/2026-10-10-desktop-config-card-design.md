# 桌面端 `/config` 命令输出改造为「配置卡片」— 设计

日期：2026-10-10
状态：已批准（方案 A）
范围：`desktop`（前端）

## 1. 背景与问题

桌面端的 `/config` 命令（`desktop/src/App.tsx:1295`）读取宿主配置后，把
`config/read` 返回的每个字段用 `key: value` 拼成**一段纯文本**，交给
`session.notice(...)` 塞进会话流：

```ts
const cfg = await c.request<Record<string, unknown>>("config/read", {});
session.notice(
  Object.entries(cfg).map(([k, v]) => `${k}: ${String(v)}`).join("\n"),
);
```

渲染落在 `desktop/src/components/ChatView.tsx:86` 的 `notice` 分支——一个居中
灰色小胶囊：

```tsx
<div className="my-1 self-center rounded-md bg-raised/60 px-3 py-1 text-xs text-fg-muted">
  {item.text}
</div>
```

问题：

1. **无结构**：12 个字段挤在一段等宽文本里，字段对的边界靠肉眼找 `:`，没有分组。
2. **机器语汇直出**：`sandbox` 是 Rust 的 `{:?}` 调试格式（`WorkspaceWrite`），
   布尔打印成 `true` / `false`，与桌面上其它地方的人话化写法（`ModeChip` 的
   `normal` / `yolo`）不一致。
3. **窄屏撑破**：`workdir` 是长绝对路径，纯文本行会在窄窗口里横向溢出。

用户诉求：把 `/config` 的输出改成一张**卡片**，看起来更像一个信息面板。

## 2. 决策（方案 A：常驻展开的紧凑卡片）

把 `/config` 的输出从「一段文本 notice」改成「一个结构化卡片」，仍然是会话流里的
**内联条目**（与 `/help`、`/cost` 的输出并列，随历史滚动），再跑一次就再插一张。
卡片带标题行「当前配置」，下面按三组分小标题的键值行。

```
┌ 当前配置 ─────────────────────────────────┐
│ 模型与服务                                │
│   Provider    anthropic                   │
│   API URL     https://api.anthropic.com   │
│   API Key     已设置（***）                │
│   模型        claude-sonnet-4             │
│                                           │
│ 运行环境                                  │
│   工作目录    /Users/me/project           │
│   沙箱        workspace-write             │
│   沙箱可提升  是                          │
│   YOLO        否                          │
│                                           │
│ 限额                                      │
│   最大轮次    20                          │
│   常驻子代理   8                          │
│   直接子代理   4                          │
│   压缩阈值    160000                      │
└───────────────────────────────────────────┘
```

分组与标签映射（`config/read` → 卡片）：

| 分组 | 字段 | 中文标签 | 值处理 |
| --- | --- | --- | --- |
| 模型与服务 | `provider` | Provider | 原样 |
| | `api_url` | API URL | 原样，长值 `break-all` |
| | `api_key` | API Key | `""` → 「未设置」；否则「已设置（***）」 |
| | `model` | 模型 | 原样 |
| 运行环境 | `workdir` | 工作目录 | 原样，`break-all` |
| | `sandbox` | 沙箱 | 映射为 `read-only` / `workspace-write` / `danger-full-access` |
| | `sandbox_promotable` | 沙箱可提升 | 布尔 → 「是」/「否」 |
| | `yolo` | YOLO | 布尔 → 「是」/「否」 |
| 限额 | `max_turns` | 最大轮次 | 原样 |
| | `max_resident_subagents` | 常驻子代理 | 原样 |
| | `max_direct_children` | 直接子代理 | 原样 |
| | `compact_threshold` | 压缩阈值 | 原样 |

## 3. 方案细节

### 3.1 结构（新增 `desktop/src/components/ConfigCard.tsx`）

```tsx
export function ConfigCard({ config, error }: { config?: Record<string, unknown> | null; error?: string | null }) {
  return (
    <div
      data-testid="config-card"
      className="my-2 self-stretch rounded-lg border border-line-strong bg-surface px-3 py-2 text-sm"
    >
      <div className="mb-1 font-medium text-fg">当前配置</div>
      {error ? (
        <div className="text-red-300">{`读取配置失败: ${error}`}</div>
      ) : (
        <div className="flex flex-col gap-2">
          {GROUPS.map((group) => (
            <div key={group.title} className="flex flex-col gap-0.5">
              <div className="text-xs text-fg-subtle">{group.title}</div>
              {group.rows.map((row) => (
                <div key={row.label} className="flex gap-3">
                  <span className="w-24 shrink-0 text-fg-muted">{row.label}</span>
                  <span className="min-w-0 break-all text-fg">{row.value}</span>
                </div>
              ))}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
```

要点：

- 卡片沿用仓库既有卡片语言：`rounded-lg border border-line-strong bg-surface`
  （与 `ToolCallCard`、输入卡 spec 一致），语义色 token 而非字面色阶，深/浅主题自动成立。
- **左列定宽、右列可换行**：标签 `w-24 shrink-0`，值 `min-w-0 break-all`。
  `min-w-0` 解掉 flex item 默认 `min-width: auto`，`break-all` 让长路径换行而不撑破卡片。
- 分组只需一张静态表 `GROUPS`（`{ title, rows: [{ label, key, render? }] }`），
  未知字段忽略——卡片只呈现已知的 12 个字段，避免配置结构演进时摊出机器键名。
- **缺失字段**：某键不在响应里时，该行显示 `—`（而不是留空或打印 `undefined`）。
  这是配置被裁剪/版本不匹配时的诚实呈现。

### 3.2 值的人话化（纯函数，便于单测）

```ts
/** 布尔 → 中文。 */
const yesNo = (v: unknown) => (v === true ? "是" : v === false ? "否" : "—");

/** 沙箱：Rust `{:?}` 名 → 线协议 kebab 名（与 yi-agent-tools::SandboxMode::as_str 一致）。 */
const SANDBOX_LABELS: Record<string, string> = {
  ReadOnly: "read-only",
  WorkspaceWrite: "workspace-write",
  DangerFullAccess: "danger-full-access",
};
```

- `sandbox`：先查 `SANDBOX_LABELS`；查不到就用原串（未来新增模式不丢信息）。
- `api_key`：空串 → 「未设置」，非空 → 「已设置（***）」。原文从不经过前端——
  状态行只说「有没有」，不是凭证面板（与 `SettingsModelsTab.effectiveKeyLabel` 同精神）。

### 3.3 接线（`App.tsx` 与 `ChatView.tsx`）

`notice` 目前是 `{ type: "notice"; id: string; text: string }`（`lib/session.ts:10`）。
本设计**不新增协议类型**，而是让 notice 携带可选的**结构化载荷**：

- `App.tsx` 的 `case "config"`：改为把 `config/read` 的原始结果（或错误）交给
  `session.noticeConfig(cfg)` / `session.noticeConfig(null, formatError(e))`。
- `lib/session.ts`：`NoticeItem` 增加可选字段 `config?: { data?: Record<string, unknown> | null; error?: string | null }`；
  新增 `noticeConfig(data, error?)`，内部 `items.push({ type: "notice", id, text: "", config: {...} })`
  （`text` 留空，供非卡片消费者/无障碍兜底时可填摘要）。
- `ChatView.tsx` 的 `notice` 分支：

```tsx
case "notice":
  return item.config ? (
    <ConfigCard config={item.config.data} error={item.config.error} />
  ) : (
    <div className="my-1 self-center rounded-md bg-raised/60 px-3 py-1 text-xs text-fg-muted">
      {item.text}
    </div>
  );
```

`/help`、`/cost` 等其余命令仍走原胶囊，行为逐字节不变。

### 3.4 不变项 / YAGNI

- 不改 `config/read` 的服务端形状与 `redacted_view`（`yi-agent-runtime/src/config.rs:382`）。
- 不做可折叠/可关闭（用户已选常驻展开）；不新增浮层或常驻状态。
- 不引入"复制配置"、"跳转设置"等按钮。
- 不改 `/config` 的命令目录描述（`lib/slash.ts` 的「显示当前配置」）。
- 不新增协议 item 类型；卡片完全在客户端合成。

## 4. 测试

前端（vitest，`desktop/src`）：

- 新增 `components/ConfigCard.test.tsx`：
  1. 标题「当前配置」与三个分组标题都在。
  2. 字段人话化：`sandbox: "WorkspaceWrite"` → 卡片内出现 `workspace-write`；
     `yolo: true` / `sandbox_promotable: false` → 「是」「否」；
     `api_key: ""` → 「未设置」，`api_key: "***"` → 「已设置（***）」。
  3. 缺失键显示 `—`，未知键不渲染。
  4. `error` 传入时显示「读取配置失败: …」且不渲染字段表。
  5. 长 `workdir` 所在的**值元素**含 `break-all`（断类名契约，jsdom 不编译 Tailwind）。
- 新增/扩展 `App.test.tsx`：跑一次 `/config`（`state.dataSources["config/read"]` 注入样本），
  断言出现 `data-testid="config-card"` / 标题「当前配置」与 `workspace-write`，
  且**不触发** `turn/start`；失败路径（`state.rejectCode["config/read"]`）显示「读取配置失败」。
- 既有用例保持通过：`/help`、`/cost` 的胶囊 notice 用例（`App.test.tsx:770`）不变。

## 5. 验证命令

- `cd desktop && npx tsc --noEmit`
- `cd desktop && npx vitest run`
- 视觉：`cd desktop && npm run dev`，看深/浅两套主题下 `/config` 卡片。

## 6. 人工验收清单

- [ ] 桌面：输入 `/config`，会话流里出现一张带边框的卡片，标题「当前配置」。
- [ ] 三个分组（模型与服务 / 运行环境 / 限额）清晰可辨，键值对齐。
- [ ] 沙箱显示 `workspace-write` 等 kebab 名；布尔显示「是 / 否」；api_key 显示「已设置（***）/ 未设置」。
- [ ] 窄窗口（约 720px）下长 `workdir` 换行而不是撑破卡片。
- [ ] 深/浅主题下边框、底色、文字对比度都成立。
- [ ] `config/read` 失败时卡片显示「读取配置失败: …」，其余命令（`/help`、`/cost`）仍是原胶囊。
