# 桌面端「设置 → 运行参数」排版重做 设计

**日期**: 2026-11-01
**状态**: 待实现
**相关代码**: `desktop/src/components/SettingsRuntimeTab.tsx`、`desktop/src/components/SettingsRuntimeTab.test.tsx`、`desktop/src/lib/runtimeSettings.ts`、`desktop/src/index.css`、`desktop/src/lib/errorMessage.ts`

## 1. 背景

### 1.1 用户报的问题

> desktop App 设置界面里面的运行参数部分实在是太丑陋了。

「丑」是可复现的，不是主观抱怨。下面每一条都有代码、构建产物或浏览器实测证据。

### 1.2 根因一：整个页面没有一个样式类存在

`SettingsRuntimeTab.tsx` 用五个类名搭骨架：`settings-runtime`（根）、`settings-group`（分组）、`settings-row`（一行）、`settings-hint`（说明文字）、`settings-error`（错误文字）。

全仓 grep 的结果是：**这五个类名在 `desktop/src/index.css`（534 行，唯一的样式入口）与任何其它 CSS 文件里都不存在**，只命中组件自身与 `docs/superpowers/plans/2026-10-31-remove-dotenv-settings-json.md`（当初的规划稿，样式一并漏写了）。

后果：

- `.settings-panel`（`SettingsDialog.tsx:328`）只有 `min-h-0 flex-1 overflow-y-auto`，**没有 padding**。其它 Tab 自己带 `p-5`（`SettingsGeneralTab.tsx:35`、`SettingsTagsTab.tsx:61`、`SettingsRemoteTab.tsx:196`），运行参数页没有，于是内容贴着面板左上角起排。
- `<label>`、`<input>`、`<p>`、`<button>` 全是 block 元素，各占一整行、零间距。
- `<input>` / `<select>` / `<button>` 是**系统原生的灰色控件**，与全站 `rounded-md border border-line` 的视觉语言脱节。

### 1.3 根因二：每个字段是 5 层竖向堆叠

`SettingsRuntimeTab.tsx:95-141`，一个字段在 DOM 里是：

```
label            「最大轮次」
input            空框，placeholder = 「未设置 · 默认 200」
p.settings-hint  「未设置 · 默认 200」      ← 与 placeholder 逐字重复
p.settings-hint  「一次对话里 agent 最多执行的轮数」
button           「恢复默认」               ← 未设置时是 disable 的，全 12 行都长这样
```

12 个字段 × 5 行 = 一堵 60 行的墙。两个具体冗余：

1. **同一句话写两遍**：placeholder 与下面那行 hint 都是 `未设置 · 默认 ${field.default}`。
2. **12 颗常驻的禁用按钮**：未设置时「恢复默认」不可点，但仍占一整行、仍画出来。

### 1.4 根因三：分组标题会漏出英文，且顺序错

`SettingsRuntimeTab.tsx:9-15`：

```ts
const GROUP_LABELS = { agent: "Agent", compact: "压缩", startup: "启动" };
const GROUP_ORDER = ["agent", "compact", "startup"];
```

主分支 `13510f48` 之后，`yi-agent-runtime/src/settings.rs` 新增 `tui` 分组与三个字段（`tui.notifications` / `tui.notification_method` / `tui.notification_condition`）。三者都不在上表里，于是：

- 标题直接显示英文 `tui`（`GROUP_LABELS[group] ?? group`）；
- 不在 `GROUP_ORDER` 里，被 `:56-59` 的 `extra` 分支踢到**最后**，排在「启动」之后。

当前后端 `FIELDS` 共 12 个字段，分组分布（`grep -c 'FieldSpec {'` 与 `grep -o 'group: "[a-z]*"'` 实测）：`agent` 5、`compact` 3、`tui` 3、`startup` 1。

### 1.5 根因四：路径拿到了却扔掉

`settings/read` 的响应体带 `path`（`yi-agent-app-server/src/settings_rpc.rs:39-42`，真实 `settings.json` 绝对路径）。`parseRuntimeSettings` 也保留了它（`runtimeSettings.ts:86`）。但组件只取 `view.fields`（`:37`），`view.path` 从未使用。这个页面改的是本机一个真实文件，路径是最有用的上下文之一却不可见。

### 1.6 根因五：错误信息会渲染成 `[object Object]`

`SettingsRuntimeTab.tsx:45,76` 用 `String(error)` 把异常转成文案。

但 RPC 失败时 `request()` reject 的是**服务端原样返回的 `RpcError` 对象**（`lib/rpc.ts:268` `pending.reject(msg.error)`；`RpcError = { code, message, data? }`，见 `lib/generated/RpcError.ts:3`）。对普通对象 `String()` 得 `"[object Object]"`。

所以「运行参数」页在保存被拒时，用户看到的是 `[object Object]` 而不是 `invalid setting agent.max_turns: must be at most 4294967295`。

仓库对此早有结论：`lib/errorMessage.ts` 的 `formatError` 第一行注释就是「RPC rejections are `RpcError` objects, so `String(e)` would render `[object Object]`」，并在 7 处使用（`App.tsx:42`、`hooks/useSettingsState.ts:4` 等）。**本页是漏网的那一处。**

这一条不是「排版」问题，但它和排版同处一个组件、同一批测试，且修复只需换一个函数调用，故并入本次范围。

### 1.7 可用 token 比预期少

`index.css` 的 `@theme inline` 暴露 14 个 token。对**当前构建产物** `dist/assets/index-CcngtJOm.css` 实测（`grep -o "\.<name>[{,: ]"`）：

| 类名 | 产物中存在 |
|---|---|
| `bg-surface` / `bg-panel` / `bg-raised` | 是 |
| `text-fg` / `text-fg-muted` / `text-fg-subtle` / `text-fg-faint` | 是 |
| `border-line` / `border-line-strong` | 是 |
| **`bg-base`** | **否** |
| **`bg-accent`** | **否** |
| **`text-accent`** | **否** |

`bg-base` / `bg-accent` / `text-accent` 根本不存在，因为 `@theme inline` 里没有 `--color-base` / `--color-accent`。而 `SettingsWebSearchTab.tsx:165,176,190,201` 在用 `bg-base`，`:222` 用 `bg-accent`，`:239` 用 `text-accent`——那几处**实际没有背景色，主按钮也没有强调色**。

这是另一个既有缺陷（网页搜索页），**不在本次范围**，但它给本次立了一条硬约束：**只用上表里「是」的那些 token，并且实现后必须回产物里逐个 grep 确认**。

## 2. 目标与非目标

### 2.1 目标

1. 与「通用」「标签」「远程访问」三页同语言：同 padding、同边框、同控件尺寸。
2. 一个字段压缩成**一行**（左标签+描述，右控件），12 个字段可快速纵向扫描。
3. 状态与操作有明确层级：默认值 / 自定义值一眼可分；「恢复默认」只在**可操作**时出现。
4. 底部有稳定操作区，`settings.json` 真实路径可见，无改动时保存不可点。
5. 分组标题全中文，`tui` 出现在正确位置。
6. 枚举值显示中文，提交值仍是后端认的原始字符串。
7. 错误信息可读（`formatError`），不再出现 `[object Object]`。

### 2.2 非目标

- **不改后端**。`settings/read|write` 协议、`FIELDS` 元数据、`buildPatch` 语义一律不动。
- **不改三态语义**。`value === null` 仍是「未设置」，仍绝不归一成默认值（`runtimeSettings.ts:7-9` 的理由不变）。
- **不动其它设置页**，包括 `SettingsWebSearchTab` 里 `bg-base` / `bg-accent` 不生效（另开）。
- **不引入 UI 组件库、不新增依赖、不新增 CSS 文件**。
- **`index.css` 只改一处**：给移动端 16px 选择器补 `textarea`（§3.11 说明了为什么不能只靠组件里的类名解决）。其它任何 `index.css` 改动都超出范围。

## 3. 设计

以下每个类名组合都在真实 Tailwind v4.3.3 + 本项目 `index.css` 下生成过产物并截图验证（方法见 §5）。

### 3.1 断点必须用容器查询，不能用 `sm:` / `md:`

**这是本次设计最关键的一条，写错过一次，实测推翻了。**

设置弹窗是 `w-[70vw] max-w-3xl` 且左栏固定 `w-40`（`SettingsDialog.tsx:223,235`）。内容区宽度 = `min(70vw, 768) - 160 - 40`：

| 视口 | 内容区 |
|---|---|
| 900px（`minWidth`） | 430px |
| 1000px | 500px |
| 1200px 及以上 | 568px（已撞 `max-w-3xl`） |

也就是说**内容区永远不会超过 568px**，而视口断点 `md:`(768) / `lg:`(1024) 量的是视口，不是内容区。

无头 Chrome 实测（同一 360px 内容宽度、视口 900px）：

- 用 `md:flex-row`：左列被压到 ~100px，「压缩保留用户 token」折成 **3 行**、描述折成 **5 行**——比单栏还丑。
- 用 `@md:flex-row`（容器查询，28rem=448px）：正确保持单栏。

再测左右分栏的**最小可行内容宽度**（标签不折行）：

| 方案 | 触发内容宽度 | 触发时左列宽 | 最长标签 |
|---|---|---|---|
| `@md:` | 448px | **176px** | 「压缩保留用户 token」折行，行高 74→81px |
| `@lg:` | 512px | **240px** | 单行，行高稳定 74px |

**结论：用 `@lg:`（容器查询 32rem = 512px）**，容器加在每组的卡片上（`@container`），而不是外层 padding 容器——实测把 `@container` 放在带 `p-5` 的包裹层上时，阈值会把 padding 算进容器宽度，触发点提前 40px，同样会挤出折行。

桌面壳 `minWidth: 900`（`src-tauri/tauri.conf.json`）→ 内容区 430px < 512px，**默认窗口下是单栏**；窗口拉宽到约 1020px 以上才分栏。这是正确的取舍：单栏在 430px 下排得开、可读性优先。

容器查询的浏览器门槛是 Chrome 105 / Safari 16（macOS 13）。Tauri 桌面壳用系统 WKWebView，故 macOS 13+ 才有此能力；`index.css` 已有的 `@container` 类写法在产物里已验证可生成（`@container (width >= 32rem)`）。**降级行为**：不支持容器查询的旧 WebView 忽略 `@lg:*` 规则，退回单栏——即默认堆叠布局，不会破版。

### 3.2 页面骨架

```tsx
<div className="flex min-h-full flex-col">        {/* 撑满 panel，底栏才能贴底 */}
  <div className="flex-1 space-y-6 p-5">          {/* 与其它 Tab 一致的 p-5 */}
    {/* notice：仅在有消息时出现 */}
    <section>
      <h3 className="text-sm font-medium text-fg">Agent</h3>
      <p className="mt-1 text-xs text-fg-subtle">单次对话的执行上限与提示词覆盖</p>
      <div className="@container mt-3 divide-y divide-line rounded-md border border-line">
        {/* 每字段一行 */}
      </div>
    </section>
    {/* 其余分组 */}
  </div>
  <div className="sticky bottom-0 ...">{/* 底栏 */}</div>
</div>
```

`min-h-full`（产物里是 `min-height:100%`）+ `flex-1`：内容不足一屏时底栏仍贴面板底部。无头 Chrome 实测：面板 260px、内容仅 3 行时，底栏下沿 = 面板底边，不乱跑。

### 3.3 组名、组说明与顺序

```ts
const GROUP_LABELS: Record<string, string> = {
  agent: "Agent",
  compact: "上下文压缩",
  startup: "启动",
  tui: "终端通知",
};

/** 每组一句话说明在管什么。查不到就不渲染，不造空话。 */
const GROUP_HINTS: Record<string, string> = {
  agent: "单次对话的执行上限与提示词覆盖",
  compact: "上下文接近上限时的自动压缩策略",
  tui: "命令行界面在回合结束时的提示方式",
  startup: "启动时生效的默认档位",
};

const GROUP_ORDER = ["agent", "compact", "tui", "startup"];
```

`GROUP_ORDER` 按影响面排：agent 是主循环、compact 是它的兜底、tui 是另一个前端、startup 是启动期一次性档位。

`agent` 保留英文：它是本项目核心术语，全站（设置导航、TUI、文档）都写 `Agent`，译成「智能体」反而生造。其余三个中文。

`extra` 兜底逻辑保留：将来后端新增分组时，显示原始英文名并排在最后，而不是消失。

### 3.4 单行字段

```tsx
<div className="flex flex-col gap-2 px-3 py-2.5 @lg:flex-row @lg:items-start @lg:justify-between @lg:gap-6">
  <div className="min-w-0 @lg:flex-1">
    <label htmlFor={id} className="block text-sm text-fg">{field.label}</label>
    <p className="mt-0.5 text-xs leading-relaxed text-fg-subtle">{field.description}</p>
  </div>
  <div className="shrink-0 @lg:w-56">
    {控件}
    <div className="mt-1 flex items-center justify-between gap-2 text-xs">{状态}{恢复默认}</div>
  </div>
</div>
```

- 控件统一：`w-full rounded border border-line-strong bg-raised px-2.5 py-1.5 text-sm text-fg outline-none focus:border-fg-subtle disabled:opacity-50`
- `htmlFor` / `id` 关联保持现状（`runtime-${field.key}`），`<label>` 仍是控件可访问名——**现有 `getByLabelText("最大轮次")` 必须继续通过**。
- 描述用 `text-fg-subtle`（3.78:1）。它低于 WCAG AA 的 4.5:1，**这是刻意的**：描述是辅助信息、与全站其它 `text-xs text-fg-subtle` 一致（`SettingsTagsTab.tsx:64`、`SettingsRemoteTab.tsx:198`）。改成 `fg-muted`（7.11:1）会让人误以为描述和标签同级，反而破坏层级。若日后要提对比度，应整体调整 `--fg-subtle`，不是这一页单独改。

### 3.5 为什么控件用 `bg-raised` + `border-line-strong`

初版设计写的是 `bg-surface` + `border-line`（跟网页搜索页抄）。**截图推翻了它**：

| 组合 | 深色实测观感 |
|---|---|
| `bg-surface`(#0a0a0a) + `border-line`(#262626) | 输入框是一块比卡片**更黑**的洞，边框几乎不可见 |
| `bg-raised`(#262626) + `border-line-strong`(#404040) | 控件从卡片上「浮」出来，边界清楚 |

对比度实测（对相邻 `panel` #171717）：`bg-surface` 仅 1.10:1，`bg-raised` 1.18:1；边框 `line` 1.18:1 vs `line-strong` 1.73:1。数值差距小，但**方向相反**（surface 比 panel 暗 = 凹陷，raised 比 panel 亮 = 凸起），视觉上「凹陷」读作只读区，「凸起」才读作可编辑控件。浅色主题下同样成立（已截图确认）。

这不是新发明：`MessageInput`、`ThreadSidebar` 的输入框用的是 `bg-surface`，但它们的父容器是**更亮的 `panel`**、且外围是 `bg-transparent` 大卡片，语境不同。本页的父容器是 `panel`、卡片边界靠 `border-line` 区分，用 `raised` 才有层次。

### 3.6 状态与被消除的重复

| 情形 | 右侧第二行 | 恢复默认 |
|---|---|---|
| `value === null` | `默认 200`（`text-fg-subtle`） | **不渲染** |
| 已设置 | `已设为 500`（`text-fg-muted`） | 行内小字按钮，可点 |

- 输入框**不再带 placeholder**：原来 placeholder 与状态行讲同一句话（§1.3），现在只留状态行。
- `select` 首项文案 `未设置（默认 工作区可写）`，枚举走 §3.7 的中文映射。
- 「恢复默认」未设置时不渲染（不是 `disabled`）。12 行里 0 颗禁用按钮。
- 保存按钮在无改动时仍是 `disabled`——它表达「现在没东西可存」，是合理禁用。

### 3.7 枚举值中文映射

```ts
/** 只影响显示；提交值永远用原始字符串。未知值原样返回（后端加选项时不至于空白）。 */
const VALUE_LABELS: Record<string, Record<string, string>> = {
  "startup.sandbox": {
    "read-only": "只读",
    "workspace-write": "工作区可写",
    "danger-full-access": "完全访问",
  },
  "tui.notifications": { true: "开", false: "关" },
  "tui.notification_method": { auto: "自动", osc9: "OSC 9（带正文）", bel: "响铃" },
  "tui.notification_condition": { unfocused: "仅窗口失焦时", always: "总是" },
};
```

`labelFor(key, raw)` 统一作用于 `options`、`default_display`、当前值三处。`default_display` 也必须映射——否则沙箱字段未设置时显示 `默认 workspace-write`，旁边下拉框却是中文，同屏两种语言。

未列入映射的字段（数字、文本、将来新增的 select）原样返回，不报错、不留空。

### 3.8 底部操作条

```tsx
<div className="sticky bottom-0 flex flex-wrap items-center justify-between gap-3 border-t border-line bg-panel px-5 py-3">
  <span className="min-w-0 truncate text-xs text-fg-subtle" title={path}>settings.json · {path}</span>
  <div className="flex shrink-0 items-center gap-2">
    <button className="rounded border border-line-strong px-3 py-1.5 text-sm text-fg-muted disabled:opacity-50">撤销改动</button>
    <button className="rounded bg-fg px-3 py-1.5 text-sm text-surface disabled:opacity-50">保存</button>
  </div>
</div>
```

- `sticky bottom-0` + `bg-panel`：面板是唯一滚动容器，底栏吸在其底部；同色防止内容透出。
- 路径 `text-fg-subtle` + `truncate` + `title`：次要信息不抢焦点，悬停可读全。不换行——换行会把底栏撑成两行。
- **主按钮 `bg-fg text-surface`**：反色，深浅两套主题下对比度 18.16:1 / 17.93:1。仓库已有先例（`LeadAvatar.tsx:15`、`SuperpowersKanbanPluginSettings.tsx:205`）。不用 `bg-accent`——它不存在（§1.7）。
- **撤销改动**：把 `drafts` 重置回各字段当前值。文案不用「重置」（有歧义：重置为默认值？）。
- 两者都由 `buildPatch(fields, drafts)` 是否为空控制 `disabled`。

**为什么保存放底部不是每组一颗**：`settings/write` 收一份 patch、一次提交所有改动。每组一颗保存会让人以为分组有独立事务。

移动端：面板自带 `padding-bottom: env(safe-area-inset-bottom)`（`index.css:403`）。无头 Chrome 实测，`sticky bottom-0` 的底栏停在安全区之**上**（面板底 250 / 安全区上沿 216 / 底栏下沿 215），不会沉进 Home Indicator。故**不需要**额外处理。

### 3.9 notice 与错误可读性

```tsx
<p role={notice.ok ? "status" : "alert"}
   className={`rounded-md border px-3 py-2 text-xs ${
     notice.ok ? "border-line bg-raised/50 text-fg-muted"
               : "border-amber-500/40 bg-amber-500/10 text-amber-500"}`}>
```

- `role` 按成功/失败区分（现状恒定 `status` 是缺陷：失败信息不该被读屏当普通状态）。
- `amber-500` 是既有告警色（`SettingsGeneralTab.tsx:74`、`SettingsWebSearchTab.tsx:212`）。
- **`String(error)` 全部换成 `formatError(error)`**（§1.6）。三处：`load`、`save` 的 catch，以及 notice 文案拼接。

### 3.10 空列表

`fields` 为空时渲染 `<p className="p-5 text-sm text-fg-subtle">没有可配置的运行参数</p>`，而不是一片空白底栏。

### 3.11 `agent.system_prompt` 用 `<textarea>`

它是 12 个字段里唯一的自由文本（`FieldType::Text`），其余 11 个是数字或枚举。系统提示词动辄几百字，塞进单行 `<input>` 只能左右滚动，无法通读——这正是「难看」的一部分。

```tsx
<textarea
  id={id}
  rows={3}
  className="w-full resize-y rounded border border-line-strong bg-raised px-2.5 py-1.5 text-sm text-fg outline-none focus:border-fg-subtle"
  value={drafts[field.key] ?? ""}
  onChange={...}
/>
```

- `resize-y`：允许纵向拉伸，横向不拉（避免撑破卡片）。
- **`<textarea>` 不设 `placeholder`**，与 §3.6 一致。
- 该字段所在行用 `@lg:items-start`（已有），textarea 顶部与标签顶部对齐。

**代价**：`index.css:433` 的移动端 16px 字号规则只选了 `input, select`，不含 `textarea`。iOS 上聚焦会触发自动放大。故本次一并把 `textarea` 加进那条选择器：

```css
[data-mobile="true"] .settings-panel input,
[data-mobile="true"] .settings-panel select,
[data-mobile="true"] .settings-panel textarea {
  font-size: 16px;
}
```

这是本次**唯一**动 `index.css` 的地方，与既有注释同源同理（`mobileLayout.test.ts:273` 断言的正则 `/(?:input|select)/` 仍匹配新选择器，已实测通过）。

### 3.12 数据流（不变）

```
mount → call("settings/read") → parseRuntimeSettings → fields[]
                                    ↓
                         drafts（从当前值起步）
                                    ↓
编辑控件 → setDrafts ──→ dirty = buildPatch(fields, drafts)
                                    ↓
                      保存 → call("settings/write", {patch}) → load()
```

`load()` 后 `drafts` 重新从 `fields` 起算，`dirty` 归零。保存后先 `load` 再 `setNotice({ok:true})`（现状 `:71-73`）。

## 4. 测试

`SettingsRuntimeTab.test.tsx` 现有 4 条，2 条断言会被本次改掉的文案：

| 现有断言 | 处理 |
|---|---|
| `findByText(/未设置/)` + `getByText(/默认 200/)` | 改为 `findByText("默认 200")`，并断言「恢复默认」**不存在** |
| `getByRole("button",{name:/恢复默认/}).disabled === true` | 删除（未设置时不渲染） |
| 已设置时 `findByText(/已设为 500/)` + 按钮可点 | 保留 |
| 点恢复默认 → `settings/write` 收 `{patch:{key:null}}` | 保留 |
| 校验失败内联展示 `must be at least 1` | 保留；**加强**为断言显示的是 message 而非 `[object Object]` |
| `SettingsDialog.test.tsx` 的 `findByText("读取中…")` | 保留该字面文案 |

新增 5 条：

1. **无改动时保存禁用**：初始 `disabled === true`；改一个输入框后 `false`；点「撤销改动」回到 `true` 且输入框恢复原值。
2. **`tui` 分组中文标题**：喂 `group:"tui"` 字段，断言 `getByText("终端通知")`，且页面**不含**文本 `tui`。
3. **枚举中文展示、原值提交**：`startup.sandbox` 未设置时显示 `默认 工作区可写`；选「完全访问」保存后 `settings/write` 收到 `"danger-full-access"`。
4. **路径可见**：`path:"/x/settings.json"` 时页面能看到它。
5. **错误对象渲染可读**：`call` reject 一个 `{code:-32602, message:"invalid setting agent.max_turns: must be at least 1"}` 对象，断言页面出现该 message，且**不出现** `[object Object]`。

不改 `runtimeSettings.test.ts`（`buildPatch` / `parseRuntimeSettings` 一行不动）。

## 5. 验证

1. `cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx src/components/SettingsDialog.test.tsx src/lib/runtimeSettings.test.ts`
2. `cd desktop && npx tsc --noEmit`
3. `cd desktop && npm run build`
4. **产物类名门禁**（手工，必须做）：对 `dist/assets/*.css` 逐个 grep 本页用到的新类名，**每一个都必须命中**。已知必查：`@container`、`divide-y`、`divide-line`、`bg-raised`、`border-line-strong`、`min-h-full`、`sticky`、`truncate`、`leading-relaxed`、`focus:border-fg-subtle`、`disabled:opacity-50`、`bg-amber-500/10`、`border-amber-500/40`。同时断言产物**不含** `bg-base` / `bg-accent` / `text-accent`（防复发）。
5. **视觉自检**：mock 12 个真实字段（含已设置、未设置、select、长描述、textarea），在 `data-theme=dark` / `light` 两种主题、窗口 1280×860 与 900×600 下各截图，人工确认：分组边界、控件左缘对齐、底栏吸底、未设置行无恢复按钮、输入框「凸起」而非「凹陷」。
6. 同步 `docs/project-management/desktop.md` 的运行参数条目（可验证判据 + 验证命令），同一 commit 提交。

## 6. 风险

| 风险 | 应对 |
|---|---|
| 断点用错（`sm:`/`md:` 视口断点） | §3.1 已实测推翻，改用容器查询 `@lg:`；容器挂在卡片上而非 padding 层 |
| 旧 WebView 不支持容器查询 | 降级为单栏堆叠，不破版；桌面壳 macOS 13+ 才有此能力 |
| 文案改动破坏测试 | §4 已逐条对应；先改测试再改实现 |
| 误用不存在的 `bg-base`/`bg-accent` | §1.7 硬约束 + §5.4 产物 grep 双向断言 |
| 控件用 `bg-surface` 显得像黑洞 | §3.5 已截图对比，改用 `bg-raised` + `border-line-strong` |
| `fg-subtle` 描述文字 3.78:1 低于 AA | 刻意与全站辅助文字一致，§3.4 说明；要改应整体调 token |
| 触屏 `<textarea>` 未被 16px 规则覆盖 | `index.css:433` 只选了 `input, select`。`agent.system_prompt` 是自由文本、需要多行编辑（§3.11），故**给该选择器补 `textarea`**——这是本次唯一允许的 `index.css` 改动，理由与既有注释同源（iOS 聚焦 <16px 输入框会放大页面）。已验证现有测试正则 `/(?:input|select)/` 仍匹配、断言仍通过 |
| 保存后 `formatError` 改变既有文案 | 只把 `String(e)` 换成 `formatError(e)`，成功路径文案不变 |
