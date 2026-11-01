# 桌面端「设置 → 运行参数」排版重做 设计

**日期**: 2026-11-01
**状态**: 待实现
**相关代码**: `desktop/src/components/SettingsRuntimeTab.tsx`、`desktop/src/components/SettingsRuntimeTab.test.tsx`、`desktop/src/lib/runtimeSettings.ts`、`desktop/src/index.css`

## 1. 背景

### 1.1 用户报的问题

> desktop App 设置界面里面的运行参数部分实在是太丑陋了。

「丑」是可复现的，不是主观抱怨。下面每一条都有代码或产物证据。

### 1.2 根因一：整个页面没有一个样式类存在

`SettingsRuntimeTab.tsx` 用五个类名搭骨架：`settings-runtime`（根）、`settings-group`（分组）、`settings-row`（一行）、`settings-hint`（说明文字）、`settings-error`（错误文字）。

全仓 grep 的结果是：**这五个类名在 `desktop/src/index.css`（534 行，唯一的样式入口）与任何其它 CSS 文件里都不存在**，只命中组件自身与 `docs/superpowers/plans/2026-10-31-remove-dotenv-settings-json.md`（那是当初的规划稿，样式一并漏写了）。

后果是它们退化成 `class` 属性上的字符串，浏览器的表现是：

- `.settings-panel`（`SettingsDialog.tsx:328`）只有 `min-h-0 flex-1 overflow-y-auto`，**没有 padding**。其它 Tab 自己带 `p-5`（`SettingsGeneralTab.tsx:35`、`SettingsTagsTab.tsx:61`、`SettingsRemoteTab.tsx:196`），运行参数页没有，于是内容贴着面板左上角起排。
- `<label>`、`<input>`、`<p>`、`<button>` 全是 block 级元素，各占一整行、零间距。
- `<input>` / `<select>` / `<button>` 是**系统原生的灰色控件**，与全站 `rounded-md border border-line` 的视觉语言完全脱节。

### 1.3 根因二：每个字段是 5 层竖向堆叠

`SettingsRuntimeTab.tsx:95-141`，一个字段在 DOM 里是：

```
label            「最大轮次」
input            空框，placeholder = 「未设置 · 默认 200」
p.settings-hint  「未设置 · 默认 200」      ← 与 placeholder 逐字重复
p.settings-hint  「一次对话里 agent 最多执行的轮数」
button           「恢复默认」               ← 未设置时是 disable 的，全 12 行都长这样
```

12 个字段 × 5 行 = 一堵 60 行的墙。扫描任何一项都要先跳过三行噪声。

两个具体的冗余：

1. **同一句话写两遍**：placeholder 与下面那行 hint 都是 `未设置 · 默认 ${field.default}`。
2. **12 颗常驻的禁用按钮**：未设置时「恢复默认」不可点，但它仍然占一整行、仍然画出来。禁用控件不该是这个页面的常态视觉。

### 1.4 根因三：分组标题会漏出英文，且顺序错

`SettingsRuntimeTab.tsx:9-15`：

```ts
const GROUP_LABELS = { agent: "Agent", compact: "压缩", startup: "启动" };
const GROUP_ORDER = ["agent", "compact", "startup"];
```

主分支 `13510f48` 之后，`yi-agent-runtime/src/settings.rs` 新增了 `tui` 分组与三个字段（`tui.notifications` / `tui.notification_method` / `tui.notification_condition`）。三者都不在上表里，于是：

- 标题直接显示英文 `tui`（`GROUP_LABELS[group] ?? group`）；
- 不在 `GROUP_ORDER` 里，被 §3.2 的「extra」分支踢到**最后**，排在「启动」之后。

顺带：`agent` 显示成英文 `Agent`，`compact` 显示成「压缩」、`startup` 显示成「启动」——中英混排。

### 1.5 根因四：路径信息拿到了却扔掉

`settings/read` 的响应体带 `path`（`yi-agent-app-server/src/settings_rpc.rs:39-42`，真实 `settings.json` 绝对路径）。`parseRuntimeSettings` 也保留了它（`runtimeSettings.ts:86`）。但 `SettingsRuntimeTab` 只取 `view.fields`（`:37`），`view.path` 从未使用。

这个页面改的是**本机一个真实文件**，路径是最有用的上下文之一却不可见。

### 1.6 参照系：可用 token 只有这几个

`index.css` 的 `@theme inline` 把十四个 token 暴露成 Tailwind 工具类。构建产物 `dist/assets/index-CcngtJOm.css` 里能查到的是：

`surface` / `panel` / `raised` / `line` / `line-strong` / `fg` / `fg-muted` / `fg-subtle` / `fg-faint` / `diff-add-bg` / `diff-add-fg` / `diff-del-bg` / `diff-del-fg`。

**`bg-base`、`bg-accent`、`text-accent` 不在其中**（`grep -c "\.bg-accent" dist/assets/index-CcngtJOm.css` → `0`）。这一点必须写进设计约束：`SettingsWebSearchTab.tsx:165,176,190,201` 用 `bg-base`，`:222` 用 `bg-accent`，`:239` 用 `text-accent`，那几处**实际没有背景色，主按钮也没有强调色**（另一个既有问题，不在本次范围内）。

所以本页只用上表里真实存在的 token。主按钮沿用仓库已有的「反色」写法 `bg-fg text-surface`（`LeadAvatar.tsx:15`、`SuperpowersKanbanPluginSettings.tsx:205` 已用），不引入 accent。

## 2. 目标与非目标

### 2.1 目标

1. 运行参数页在视觉上与「通用」「标签」「远程访问」三页同语言（同 padding、同边框、同控件尺寸）。
2. 一个字段压缩成**一行**（左标签+描述，右控件），12 个字段可以快速纵向扫描。
3. 状态与操作有明确层级：当前是默认值 / 是自定义值，一眼可分；「恢复默认」只在**可操作**时出现。
4. 底部有稳定的操作区，`settings.json` 的真实路径可见，无改动时保存不可点。
5. 分组标题全部中文，`tui` 分组出现在正确位置。
6. 枚举值（沙箱、通知协议…）显示中文，提交值仍是后端认的原始字符串。

### 2.2 非目标

- **不改后端**。`settings/read` / `settings/write` 的协议、`FIELDS` 元数据、`buildPatch` 的语义一律不动。
- **不改三态语义**。`value === null` 仍是「未设置」，仍绝不归一成默认值（`runtimeSettings.ts:7-9` 的理由不变）。
- **不动其它设置页**，包括 `SettingsWebSearchTab` 里 `bg-base` / `bg-accent` 不生效这两处既有问题（另开）。
- **不引入 UI 组件库、不新增依赖、不新增 CSS 文件**。全部用现有 Tailwind 语义 token 的组合类名。
- **不新增 `index.css` 规则**（`[data-mobile]` 那几条已有的输入框字号规则会自动覆盖本页的新输入框，因为它们选择的是 `.settings-panel input`）。

## 3. 设计

### 3.1 页面骨架

```tsx
<div className="flex min-h-full flex-col">          {/* 撑满 panel，让底栏能贴底 */}
  <div className="flex-1 space-y-6 p-5">            {/* 与其它 Tab 一致的 p-5 */}
    {/* notice：仅在有消息时出现 */}
    <section>
      <h3 className="text-sm font-medium text-fg">Agent</h3>
      <p className="mt-1 text-xs text-fg-subtle">单次对话的执行上限与提示词</p>
      <div className="mt-3 divide-y divide-line rounded-md border border-line">
        {/* 每个字段一行 */}
      </div>
    </section>
    {/* 其余分组 */}
  </div>
  <div className="sticky bottom-0 ... ">{/* 底部操作条 */}</div>
</div>
```

分组用「标题 + 一句话说明 + 一张带分隔线的边框卡片」，而不是裸 `<h3>` 加一堆 div。边框卡片给出明确的组边界，`divide-y` 让行与行之间有细线——这是「一堵墙」与「一张表」的区别。

### 3.2 组名与组说明

```ts
const GROUP_LABELS: Record<string, string> = {
  agent: "Agent",
  compact: "上下文压缩",
  startup: "启动",
  tui: "终端通知",
};

/** 每组一句话，说明这组参数在管什么。查不到就不渲染，不要造一句空话。 */
const GROUP_HINTS: Record<string, string> = {
  agent: "单次对话的执行上限与提示词覆盖",
  compact: "上下文接近上限时的自动压缩策略",
  startup: "启动时生效的默认档位",
  tui: "命令行界面在回合结束时的提示方式",
};

const GROUP_ORDER = ["agent", "compact", "tui", "startup"];
```

`GROUP_ORDER` 按「影响面从大到小」排：agent 是主循环、compact 是它的兜底、tui 是另一个前端、startup 全局生效。`tui` 放在 `startup` 前，因为前三个都是行为策略，`startup` 是启动期的一次性档位。

`agent` 保留英文名：它是本项目的核心术语，全站（`SettingsDialog`、TUI、文档）都写 `Agent`，强行译成「智能体」反而生造。其余三个中文。

§3.2 的「未知分组」兜底逻辑（`extra`，`SettingsRuntimeTab.tsx:56-59`）保留：将来后端再加 `foo` 分组时，它显示原始英文名并排在最后，而不是消失。

### 3.3 单行字段

```tsx
<div className="flex flex-col gap-2 px-3 py-2.5 sm:flex-row sm:items-start sm:justify-between sm:gap-6">
  {/* 左：标签 + 描述 */}
  <div className="min-w-0 sm:flex-1">
    <label htmlFor={id} className="block text-sm text-fg">{field.label}</label>
    <p className="mt-0.5 text-xs leading-relaxed text-fg-subtle">{field.description}</p>
  </div>
  {/* 右：控件 + 状态 */}
  <div className="shrink-0 sm:w-56">
    {控件}
    <div className="mt-1 flex items-center justify-between gap-2 text-xs">
      {状态}
      {恢复默认}
    </div>
  </div>
</div>
```

要点：

- `sm:flex-row` 而不是 `flex-row`：手机端（手机设置页整屏 ~390pt）纵向堆叠，桌面端左右分栏。手机设置页是复用同一个面板的（`SettingsDialog.tsx:364`），窄屏下 `w-56` 的控件配 `flex-1` 的标签会把描述挤成每行两三个字，故必须让它在窄屏堆叠。
- 右侧固定 `sm:w-56`：12 个字段的控件左边缘对齐成一条竖线，这是「像设置页」的关键。左列用 `min-w-0 sm:flex-1` 让长描述正常换行而不撑破卡片。
- `htmlFor` / `id` 关联保持现状（`runtime-${field.key}`）。`<label>` 因此仍是控件的可访问名——**现有测试里的 `getByLabelText("最大轮次")` 必须继续通过**。

### 3.4 状态与被消除的重复

| 情形 | 右侧第二行 | 恢复默认 |
|---|---|---|
| `value === null`（未设置） | `默认 200`（`text-fg-subtle`） | **不渲染** |
| 已设置 | `已设为 500`（`text-fg-muted`） | 行内小字按钮，可点 |

- 输入框**不再带 placeholder**。原来 placeholder 与状态行讲同一句话（§1.3），现在只保留状态行一处。
- `select` 的第一项仍是「未设置」，文案改为 `未设置（默认 工作区可写）`——枚举走 §3.5 的中文映射，读起来才通顺。
- 「恢复默认」未设置时不渲染（不是 `disabled`）。12 行里 0 颗禁用按钮。已设置时它是一颗 `text-xs text-fg-subtle hover:text-fg` 的文字按钮，和状态行同行，不占独立一行。
- **保存按钮的禁用态仍出现在「无改动」时**，那是合理的禁用——它表达「现在没东西可存」。

### 3.5 枚举值中文映射

```ts
/** 只影响显示，提交值永远用原始字符串。未知值原样返回（后端加选项时不至于空白）。 */
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

`labelFor(key, raw)` 在 `options`、`default_display`、当前值三处统一过一遍。`default_display` 也必须映射——否则「未设置」的沙箱字段会显示 `默认 workspace-write`，而旁边下拉框里是中文，同屏两种语言。

未列入映射的字段（数字、文本、以及将来新增的 select）走原样返回，不报错、不留空。

### 3.6 底部操作条

```tsx
<div className="sticky bottom-0 flex flex-wrap items-center justify-between gap-3 border-t border-line bg-panel px-5 py-3">
  <span className="min-w-0 truncate text-xs text-fg-faint" title={path}>settings.json · {path}</span>
  <div className="flex shrink-0 items-center gap-2">
    <button …>撤销改动</button>
    <button …>保存</button>
  </div>
</div>
```

- `sticky bottom-0` + `bg-panel`：面板是唯一滚动容器（`SettingsDialog.tsx:328`），底栏吸在它底部。`bg-panel` 与 `.settings-shell` 同色，滚动内容不会从底下透出来。
- 路径用 `text-fg-faint` + `truncate` + `title`：它是次要信息，不该抢焦点，但需要时可读全。长路径不换行——换行会把底栏撑成两行。
- **保存按钮**：`buildPatch(fields, drafts)` 为空时 `disabled`，class `bg-fg text-surface`（反色主按钮，见 §1.6）。标签在保存中变「保存中…」。
- **撤销改动**：`border border-line-strong` 描边按钮，同样无改动时禁用，点了把 `drafts` 重置回各字段当前值（等于重新从 `fields` 起算）。
- 文案「撤销改动」而不是「重置」：reset 有歧义（重置为默认值？）。

**为什么把保存放底部而不是每组一个**：`settings/write` 收的是一份 patch，一次提交所有改动，不是分组提交。每组一颗保存会让用户以为分组有独立的事务。

### 3.7 notice

保留现有的单条 notice（`{ok, text}`），从原来顶部裸露的 `<p>` 变成带底色的条：

```tsx
<p role={notice.ok ? "status" : "alert"}
   className={`rounded-md border px-3 py-2 text-xs ${
     notice.ok ? "border-line bg-raised/50 text-fg-muted"
               : "border-amber-500/40 bg-amber-500/10 text-amber-500"}`}>
```

`role` 在成功时是 `status`、失败时是 `alert`（现状 `role="status"` 恒定是缺陷，失败信息不该被读屏当成普通状态）。`amber-500` 系列是本仓库既有的告警色（`SettingsGeneralTab.tsx:74`、`SettingsWebSearchTab.tsx:212`）。

### 3.8 空列表

`fields` 为空（服务端返回 `[]`，例如 RPC 还没接通）时渲染 `<p className="p-5 text-sm text-fg-subtle">没有可配置的运行参数</p>`，而不是渲染一片空白底栏。

### 3.9 数据流（不变）

```
mount → call("settings/read") → parseRuntimeSettings → fields[]
                                    ↓
                         drafts（从当前值起步）
                                    ↓
编辑控件 → setDrafts ──→ dirty = buildPatch(fields, drafts)
                                    ↓
                      保存 → call("settings/write", {patch}) → load()
```

`load()` 之后 `drafts` 重新从 `fields` 起算，`dirty` 自然归零。保存后 `setNotice({ok:true})` 的时机保持现状（先 `load` 再置位，`:71-73`）。

## 4. 测试

`SettingsRuntimeTab.test.tsx` 现有 4 条，其中 2 条断言的是**会被本次改掉的文案**：

| 现有断言 | 处理 |
|---|---|
| `findByText(/未设置/)` + `getByText(/默认 200/)` | 改为：`findByText("默认 200")`，并断言「恢复默认」按钮**不存在** |
| `getByRole("button", {name: /恢复默认/}).disabled === true` | 删除（未设置时不再渲染该按钮） |
| 已设置时 `findByText(/已设为 500/)` + 按钮可点 | 保留，断言 `disabled === false` |
| 点恢复默认 → `settings/write` 收 `{patch: {key: null}}` | 保留，不改 |
| 校验失败内联展示 `must be at least 1` | 保留，不改（顺带断言 `role="alert"`） |
| `SettingsDialog.test.tsx` 的 `findByText("读取中…")` | 保留「读取中…」这句字面文案 |

新增 4 条：

1. **无改动时保存禁用**：初始渲染后「保存」`disabled === true`；改一个输入框后 `disabled === false`；点「撤销改动」后回到 `true` 且输入框恢复原值。
2. **`tui` 分组显示中文标题**：喂一条 `group: "tui"` 的字段，断言 `getByText("终端通知")`，并断言页面上**没有**文本 `tui`。
3. **枚举中文展示、原值提交**：`startup.sandbox` 未设置时显示 `默认 工作区可写`；选中「完全访问」保存后，`settings/write` 收到的仍是 `"danger-full-access"`。
4. **路径可见**：`settings/read` 返回 `path: "/x/settings.json"` 时，页面上能看到它。

不改：`runtimeSettings.test.ts`（`buildPatch` / `parseRuntimeSettings` 一行不动）。

## 5. 验证

1. `cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx src/components/SettingsDialog.test.tsx src/lib/runtimeSettings.test.ts`
2. `cd desktop && npx tsc --noEmit`
3. `cd desktop && npm run build`（确认无新增未定义类名；构建后再 `grep` 产物 CSS 确认新用到的每个类都真的生成了）
4. **视觉自检**：临时起一个 mock 页（喂 12 个真实字段，含一个已设置、一个未设置、一个 select、一个长描述），用无头 Chrome 在深色/浅色两种 `data-theme` 下各截一张图，人工确认：分组边界清楚、控件左边缘对齐、底栏吸底、未设置行没有恢复按钮。确认后删除临时文件，不提交。
5. 同步 `docs/project-management/desktop.md` 里「运行参数」对应的条目（写清可验证的完成判据与验证命令），并在同一 commit 内提交。

## 6. 风险

| 风险 | 应对 |
|---|---|
| 改动文案破坏测试 | 已逐条列出 §4 的对应关系，先改测试再改实现（TDD 顺序见实施计划） |
| 手机端把控件挤扁 | `sm:` 断点下堆叠；手机设置页复用同一面板，测试里补一条窄屏不需要（`isMobile` 只影响 `SettingsDialog` 的导航，不影响本组件；堆叠由 CSS 断点决定，jsdom 不跑 CSS，故不写假测试） |
| `divide-line` / `bg-raised/50` 这类组合类名没被生成 | 验证第 3 步构建后 grep 产物 CSS，逐个确认 |
| 深色/浅色两套下的对比度 | 只用既有语义 token，深浅两套已各自定好值；截图自检覆盖两种主题 |
| 引用了不存在的 `bg-base` / `bg-accent`（§1.6） | 设计约束明确禁止；验证第 3 步 grep 兜底 |
