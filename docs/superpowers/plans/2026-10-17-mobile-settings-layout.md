# 手机端设置界面改全屏页 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把手机端设置从「273px 居中弹窗 + 160px 左侧竖栏」改成全屏页 + 顶部横排 Tab，内容区从 111px 提到 390px，并修掉四个 Tab 的横向溢出。

**Architecture:** 沿用项目既有的手机端隔离模式——`useIsMobile` 把答案写到 `<html data-mobile>`，组件加挂点类，外观覆写全部写在 `index.css` 的 `[data-mobile="true"]` 前缀规则里。桌面不匹配任何一条，逐字节不变。不用 `max-md:` 断点（它按视口宽度判断，会误伤 800px 的桌面最小窗口）。`aria-orientation` 与「模型」Tab 的表格/卡片二选一由 `isMobile` prop 驱动（纯语义与结构差异，CSS 表达不了）。

**Tech Stack:** React 19 + TypeScript、Tailwind CSS v4、Vitest + @testing-library/react（jsdom）、真实 Chrome 无头（CDP）做几何验证。

## Global Constraints

- **禁止在 `main` 分支改动**：本计划全部在 worktree `.worktrees/mobile-settings`（分支 `fix/mobile-settings-layout`）内执行。
- **桌面零回归**：未传 `isMobile`（默认 `false`）时 `SettingsDialog` 的 DOM 结构与类名必须与改动前一致（例外：四个新增的挂点类 `settings-shell` / `settings-header` / `settings-nav` / `settings-panel` 无条件渲染，但桌面无任何规则命中它们）。既有 13 条 `SettingsDialog` 测试与 22 条 `SettingsModelsTab` 测试必须全绿。
- **`[data-mobile]` 只由 `useIsMobile` 设置**：`SettingsDialog` 不得再调 `useIsMobile()`（避免重复 effect 与双写 `dataset`），沿用 `App.tsx:198` 已有的 `isMobile`，以 prop 传入。
- **CSS 覆写一律带 `[data-mobile="true"]` 前缀**，不加 `@media` 宽度查询。
- 所有注释与文案用中文，风格与仓库现有注释一致（说清「为什么」，不只说「是什么」）。
- 提交前在 `yi-agent-rs/` 跑 `cargo fmt --all`（本计划不碰 Rust，但仓库约定 commit 前跑一次）。
- commit message 用 conventional commits，**不写** `Co-Authored-By` 行。

---

### Task 1: `SettingsDialog` 手机端全屏页骨架

**Files:**
- Modify: `desktop/src/components/SettingsDialog.tsx`
- Modify: `desktop/src/index.css`（追加手机端规则块）
- Modify: `desktop/src/App.tsx:2329-2360`（传 `isMobile`）
- Test: `desktop/src/components/SettingsDialog.test.tsx`
- Test: `desktop/src/mobileLayout.test.ts`

**Interfaces:**
- Consumes: `App.tsx:198` 的 `const isMobile = useIsMobile()`；`index.css` 既有 `[data-mobile="true"]` 规则模式。
- Produces: `SettingsDialog` 新增可选 prop `isMobile?: boolean`（默认 `false`）。DOM 挂点类：`.settings-shell`（根 shell）、`.settings-header`（标题+关闭那一行）、`.settings-nav`（`role="tablist"` 的 nav）、`.settings-panel`（`role="tabpanel"` 的 div）。这四个类名是后续任务的 CSS 与静态门禁共同依赖的契约。

- [ ] **Step 1: 写失败测试（组件行为）**

在 `desktop/src/components/SettingsDialog.test.tsx` 的 `describe("SettingsDialog focus management & tab wiring")` 之后追加一个新 `describe`：

```tsx
// 手机端把设置从「居中弹窗 + 左侧竖栏」改成「全屏页 + 顶部横条」：导航方向随之由
// 竖变横，`aria-orientation` 必须跟着翻——这是语义，CSS 表达不了。挂点类是外观
// 覆写（index.css 里 `[data-mobile="true"] .settings-*`）的抓手，两个平台都渲染，
// 桌面无样式命中。
describe("SettingsDialog phone layout hooks", () => {
  it("flips the tablist orientation to horizontal on the phone", () => {
    render(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    expect(screen.getByRole("tablist").getAttribute("aria-orientation")).toBe("horizontal");
  });

  it("keeps the vertical orientation on the desktop", () => {
    render(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />);
    expect(screen.getByRole("tablist").getAttribute("aria-orientation")).toBe("vertical");
  });

  it("renders the four class hooks the phone CSS targets, on both platforms", () => {
    // 用 rerender 切换平台，而不是手动 cleanup()——文件顶部已有 `afterEach(cleanup)`，
    // 在用例中间自行清理会卸掉组件、让后续断言失去目标。
    const { container, rerender } = render(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    expect(container.querySelector(".settings-shell")).toBeTruthy();
    expect(container.querySelector(".settings-header")).toBeTruthy();
    expect(container.querySelector(".settings-nav")).toBeTruthy();
    expect(container.querySelector(".settings-panel")).toBeTruthy();

    // 桌面端同样渲染（类无条件存在，只是没有规则命中）
    rerender(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />);
    expect(container.querySelector(".settings-shell")).toBeTruthy();
    expect(container.querySelector(".settings-header")).toBeTruthy();
    expect(container.querySelector(".settings-nav")).toBeTruthy();
    expect(container.querySelector(".settings-panel")).toBeTruthy();
  });

  it("keeps the close button reachable on the phone (44px touch target)", () => {
    render(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={vi.fn()} />,
    );
    const close = screen.getByRole("button", { name: "关闭设置" });
    // 手机上遮罩被 CSS 隐藏，关闭只能靠这颗按钮或 Escape——触控目标不能还是桌面的
    // 小尺寸。44px 是 Apple HIG 下限，与抽屉按钮 `size-11` 同一标准。
    expect(close.className).toContain("size-11");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/components/SettingsDialog.test.tsx -t "phone layout hooks"
```

Expected: FAIL —— 四条全红（`aria-orientation` 仍是 `vertical`；`.settings-shell` 等选择器 `null`）。

- [ ] **Step 3: 给 `SettingsDialog` 加 prop、挂点类与条件类名**

在 `desktop/src/components/SettingsDialog.tsx` 签名里加 `isMobile`（放在 `open` 之后，与 `ThreadSidebar` / `MessageInput` 的参数顺序习惯一致）：

```tsx
export function SettingsDialog({
  open,
  isMobile = false,
  theme,
  onThemeChange,
  onClose,
  remoteCall,
  pluginCall,
  modelCall,
  pluginProjects,
  pluginDefaultProject,
  relayUrl,
  saveRelayUrl,
}: {
  open: boolean;
  /**
   * 手机端（配对后的 iPhone 客户端）：居中弹窗改成整屏页面。由宿主把已有的
   * `useIsMobile()` 结论传下来，本组件不重复探测——`<html data-mobile>` 只能有
   * 一个设置者，两处 effect 会重复写 `dataset`。
   *
   * 只影响两件事：导航方向（`aria-orientation`）与「模型」Tab 的表格/卡片形态。
   * 其余外观覆写全在 `index.css` 的 `[data-mobile="true"]` 前缀规则里，桌面不匹配。
   */
  isMobile?: boolean;
  theme: Theme;
  // ...其余 prop 保持不变
```

根 shell 的 `className` 加挂点类（`relative flex h-[70vh] w-[70vw] max-w-3xl overflow-hidden rounded-lg ...` 这行的开头加 `settings-shell`）：

```tsx
      <div
        role="dialog"
        aria-modal="true"
        aria-label="设置"
        className="settings-shell relative flex h-[70vh] w-[70vw] max-w-3xl overflow-hidden rounded-lg border border-line bg-panel text-fg shadow-2xl"
      >
```

nav 的 `aria-orientation` 与挂点类：

```tsx
        <nav
          role="tablist"
          aria-orientation={isMobile ? "horizontal" : "vertical"}
          onKeyDown={onTablistKeyDown}
          className="settings-nav flex w-40 shrink-0 flex-col border-r border-line bg-surface p-2"
        >
```

面板 div 加挂点类：

```tsx
          <div
            role="tabpanel"
            id={panelId(active)}
            aria-labelledby={tabId(active)}
            className="settings-panel min-h-0 flex-1 overflow-y-auto"
          >
```

关闭按钮加手机端 44px 触控目标（直接用 `size-11`，两个平台一致）：

```tsx
            <button
              type="button"
              aria-label="关闭设置"
              onClick={onClose}
              className="flex size-11 shrink-0 items-center justify-center rounded text-fg-subtle hover:text-fg"
            >
              ×
            </button>
```

> 为什么桌面也改成 `size-11`：手机上遮罩被 CSS 隐藏，关闭只能靠这颗按钮或 Escape，
> 44px 是 Apple HIG 触控下限（与抽屉按钮 `size-11` 同一标准）。桌面跟着变大到 44×44
> 属于可接受的可点性改进——现有头部栏 `py-2` + 文本行高约 32px，改用后约 44px；
> `SettingsDialog.test.tsx` 既有用例不校验头部高度，不会冲突。

- [ ] **Step 4: 在 `index.css` 追加手机端覆写**

追加到 `desktop/src/index.css` 末尾（放在既有「侧栏搜索框 16px」那条之后，保持手机端规则聚在一起）：

```css
/*
 * 手机端设置：居中弹窗 → 全屏页。
 *
 * 桌面是「70vw 居中浮层 + 160px 左侧竖栏」。390pt 上 70vw 只有 273px，而左侧栏是
 * 写死的 `w-40`（160px，不随视口缩放）——它吃掉 59%，内容面板只剩 111px，四个 Tab
 * 全部横向溢出（实测：通用 +4px、远程访问 +28px、插件 +183px、模型 +352px）。溢出
 * 还滚不到：面板是 `overflow-y-auto`，只允许纵向滚动，右侧内容永久不可达。
 *
 * 手机端改成整屏页面、导航移到顶部横排，内容因此拿到全部宽度（实测 390px）。桌面
 * 不匹配 `[data-mobile="true"]`，布局与观感逐字节不变。
 */
[data-mobile="true"] .settings-shell {
  position: fixed;
  inset: 0;
  display: flex;
  flex-direction: column;
  width: auto;
  max-width: none;
  height: auto;
  border: 0;
  border-radius: 0;
  /* 底部安全区落在下面的滚动容器上（见 `.settings-panel`），这里不铺，免得
     滚动条区域也留白。 */
  padding-bottom: 0;
}

/*
 * 导航：左侧竖栏 → 顶部横条，每格等宽。实测 320/360/375/390/430pt 五档
 * `tablist.scrollWidth == clientWidth`（零溢出）；最窄的 320pt 上每格仍有 80px，
 * 最长的「远程访问」四字单行放得下。
 */
[data-mobile="true"] .settings-nav {
  flex-direction: row;
  width: auto;
  flex-shrink: 0;
  border-right: 0;
  border-bottom: 1px solid var(--line);
}
[data-mobile="true"] .settings-nav > [role="tab"] {
  flex: 1 1 0;
  white-space: nowrap;
  text-align: center;
  padding-top: 12px;
  padding-bottom: 12px; /* 单格高 ≈ 44px，达到 Apple HIG 触控下限 */
}

/*
 * 全屏页没有浮层，遮罩无意义——隐藏它（关闭走右上角按钮或 Escape）。复用既有的
 * `data-settings-backdrop` 属性选择器，不新加类名；jsdom 里没有 `data-mobile`，
 * 该规则不生效，「点击遮罩关闭」那条既有用例不受影响。
 */
[data-mobile="true"] [data-settings-backdrop] {
  display: none;
}

/*
 * 面板：全屏页里它是唯一的滚动容器，底部安全区补在它身上——`.settings-shell` 是
 * `fixed`，脱离 `body` 的内边距，安全区必须自己补一次，滚到底时最后一行才不被
 * Home Indicator 压住。与侧栏 `aside`（见上方 `.app-sidebar aside`）同一理由与
 * 同一做法，且全链路只补这一次。
 */
[data-mobile="true"] .settings-panel {
  padding-bottom: env(safe-area-inset-bottom, 0px);
  overscroll-behavior: contain;
}

/*
 * 头部栏的顶部安全区：全屏页从 `top: 0` 起，刘海区必须有留白，否则「设置」标题与
 * 关闭按钮会落在刘海/状态栏底下。`max(..., 12px)` 兜住没有安全区的设备（横屏
 * 时安全区在左右两侧，此处回落 12px 常规留白）。
 */
[data-mobile="true"] .settings-header {
  padding-top: max(env(safe-area-inset-top, 0px), 12px);
}
```

> 为什么用挂点类而不是 `.settings-shell > div:first-child`：shell 的第一个子元素是
> `<nav>`，而头部栏嵌在第二层的包裹 div 里——位置选择器要么匹配不到（`div:first-child`
> 遇 `nav` 落空），要么写成一串脆弱的后代链。显式类名同 `.app-statusbar` 的做法一致。

对应地，在 Step 3 里给头部栏那个 div 加 `settings-header` 类：

```tsx
          <div className="settings-header flex items-center justify-between border-b border-line px-4 py-2">
```

- [ ] **Step 5: 在 `App.tsx` 把 `isMobile` 传给 `SettingsDialog`**

在 `desktop/src/App.tsx` 的 `<SettingsDialog ...>` 里，`open={settingsOpen}` 之后加一行：

```tsx
      <SettingsDialog
        open={settingsOpen}
        // 手机端把设置改成全屏页（见 index.css 的 [data-mobile="true"] .settings-shell）。
        // 复用上面已有的 `isMobile`，不在这里新探测。
        isMobile={isMobile}
        theme={theme}
```

- [ ] **Step 6: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/components/SettingsDialog.test.tsx
```

Expected: PASS —— 既有 13 条 + 新增 4 条全绿。

- [ ] **Step 7: 加静态门禁（读真实 CSS，钉住挂点类与规则的配对）**

在 `desktop/src/mobileLayout.test.ts` 末尾（最后一个 `});` 之前，即 `describe("phone layout CSS")` 块内）追加：

```ts
  it("turns the phone settings into a full-screen page with a top tab bar", () => {
    // 「设置区域过于小」的根因：弹窗 `w-[70vw]` 在 390pt 上只有 273px，左侧竖栏又是
    // 写死的 `w-40`(160px)，占去 59%，内容面板只剩 111px 且横向溢出滚不到。手机端
    // 必须整屏（`position: fixed` + `inset: 0`）并让导航横排，内容才拿得到全部宽度。
    const shell =
      /\[data-mobile="true"\]\s+\.settings-shell\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(shell).toMatch(/position:\s*fixed/);
    expect(shell).toMatch(/inset:\s*0/);
    expect(shell).toMatch(/flex-direction:\s*column/);
    // 弹窗那套尺寸必须被解除，否则 fixed 了却仍是 70vw/70vh 的小盒子。
    expect(shell).toMatch(/width:\s*auto/);
    expect(shell).toMatch(/max-width:\s*none/);
    expect(shell).toMatch(/height:\s*auto/);

    const nav = /\[data-mobile="true"\]\s+\.settings-nav\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(nav).toMatch(/flex-direction:\s*row/);
    // 竖栏那条右边框要撤掉，改由底边框分栏。
    expect(nav).toMatch(/border-right:\s*0/);

    const tab =
      /\[data-mobile="true"\]\s+\.settings-nav\s*>\s*\[role="tab"\]\s*\{([^}]*)\}/.exec(
        css(),
      )?.[1] ?? "";
    // 每格等宽：320pt 上也有 80px，四字标签不折行。
    expect(tab).toMatch(/flex:\s*1 1 0/);
    expect(tab).toMatch(/white-space:\s*nowrap/);
    // 44px 触控下限（Apple HIG）：上下各 12px 内边距 + 行高。
    expect(tab).toMatch(/padding-top:\s*12px/);
    expect(tab).toMatch(/padding-bottom:\s*12px/);
  });

  it("hides the settings backdrop on the phone", () => {
    // 全屏页没有浮层，遮罩只剩白占屏；关闭改走右上角按钮或 Escape。
    expect(css()).toMatch(
      /\[data-mobile="true"\]\s+\[data-settings-backdrop\]\s*\{[^}]*display:\s*none/,
    );
  });

  it("gives the phone settings page its own safe-area insets", () => {
    // 全屏页是 `fixed`，脱离 `body` 的内边距，顶部（刘海）与底部（Home Indicator）
    // 都必须自己补：顶部落在头部栏，底部落在唯一的滚动容器上，且各只补一次。
    const head =
      /\[data-mobile="true"\]\s+\.settings-header\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(head).toMatch(/padding-top:\s*max\(env\(safe-area-inset-top/);

    const panel =
      /\[data-mobile="true"\]\s+\.settings-panel\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(panel).toMatch(/padding-bottom:\s*env\(safe-area-inset-bottom/);

    // shell 自身不得再补底部安全区，否则与面板那条叠成两倍。
    const shell =
      /\[data-mobile="true"\]\s+\.settings-shell\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(shell).toMatch(/padding-bottom:\s*0/);
  });
});
```

> **16px 那条门禁不在本任务**：它对应 Task 3 的 CSS，本任务末尾 CSS 里还没有 `.settings-panel input { font-size: 16px }`，此处加入必然红。Task 3 会把那条测试补在同一个 `describe` 块末尾。

- [ ] **Step 8: 跑门禁测试**

```bash
cd desktop && npx vitest run src/mobileLayout.test.ts
```

Expected: PASS —— 既有 16 条 + 本任务新增 3 条全绿。

- [ ] **Step 9: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add desktop/src/components/SettingsDialog.tsx desktop/src/index.css desktop/src/App.tsx desktop/src/components/SettingsDialog.test.tsx desktop/src/mobileLayout.test.ts && git commit -m "feat(desktop): 手机端设置改全屏页 + 顶部 Tab 条"
```

---

### Task 2: 「模型」Tab 窄屏改卡片布局

**Files:**
- Modify: `desktop/src/components/SettingsModelsTab.tsx`
- Test: `desktop/src/components/SettingsModelsTab.test.tsx`

**Interfaces:**
- Consumes: Task 1 的 `.settings-panel` 挂点（卡片在面板内，无需自己处理安全区）；既有的 `suspectsMissingV1(entry)`、`startEdit(entry)`、`remove(name)`、`catalog.models: ModelEntryView[]`。
- Produces: `SettingsModelsTab` 新增可选 prop `isMobile?: boolean`（默认 `false`）；组件内新增局部渲染函数 `renderCatalogTable()` 与 `renderCatalogCards()`（不导出，仅本文件使用）。

- [ ] **Step 1: 写失败测试**

在 `desktop/src/components/SettingsModelsTab.test.tsx` 末尾（最后一个 `});` 之前）追加：

```tsx
describe("SettingsModelsTab phone layout", () => {
  it("renders cards instead of a table on the phone", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    const { container } = render(<SettingsModelsTab call={call} isMobile />);
    await screen.findByText("••••1234");

    // 5 列表格固有 463px，390pt 的面板只有 ~390px，窄屏必须换成纵向卡片。
    expect(container.querySelector("table")).toBeNull();
    expect(container.querySelector("[data-model-card]")).toBeTruthy();
  });

  it("keeps the table on the desktop", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    const { container } = render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");
    expect(container.querySelector("table")).toBeTruthy();
    expect(container.querySelector("[data-model-card]")).toBeNull();
  });

  it("keeps the card actionable — same labels, same callback names", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true },
    );
    const { container } = render(<SettingsModelsTab call={call} isMobile />);
    await screen.findByText("••••1234");

    // 限定在卡片内查找：表格形态的 <th> 也有同样的文本，不限定就会误通过（从而
    // 让这条用例失去「卡片必须自己渲染字段标签」的效力）。
    const card = container.querySelector("[data-model-card]")!;
    expect(card).toBeTruthy();
    expect(card.textContent).toContain("API 格式");
    expect(card.textContent).toContain("模型标识");
    // 操作按钮语义不变（可点、有可访问名）；44px 触控目标由 `py-2` 实现。
    expect(screen.getByRole("button", { name: "编辑" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "删除 A" })).toBeTruthy();
  });

  it("still shows the /v1 hint on a phone card", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list"
        ? {
            models: [
              { name: "suspect", provider: "openai", api_url: "https://llmapi.horizon.auto", model: "m", has_key: true, api_key_masked: "••••1" },
            ],
            default_model: null,
            subagent_model: null,
            effective: null,
          }
        : {},
    );
    const { container } = render(<SettingsModelsTab call={call} isMobile />);
    // 等卡片出现再断言。不用 `findByText("suspect")`——模型名还会渲染成下方两个
    // 下拉的 <option>，按文本找会命中多个（表格形态的既有用例正是靠 `selector: "td"`
    // 才避开这点；卡片形态没有 td）。
    await waitFor(() => expect(container.querySelector("[data-model-card]")).toBeTruthy());
    expect(screen.getAllByText(/以 \/v1 结尾/)).toHaveLength(1);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/components/SettingsModelsTab.test.tsx -t "phone layout"
```

Expected: FAIL —— 四条全红。`isMobile` 还未被组件接受（TS 会报 prop 不存在，运行时该 prop 被忽略），清单始终走表格分支：`[data-model-card]` 查不到（第 1、3、4 条红）、桌面那条虽有表格但 `[data-model-card]` 的 `toBeNull()` 与 `table` 的 `toBeTruthy()` 都会通过（第 2 条绿，属预期）。

- [ ] **Step 3: 实现卡片渲染**

在 `desktop/src/components/SettingsModelsTab.tsx` 里，给组件签名加 `isMobile`：

```tsx
export function SettingsModelsTab({
  call,
  isMobile = false,
}: {
  call?: ModelsCall;
  /** 手机端（窄屏）：5 列表格换成纵向卡片，见下方 `renderCatalogCards`。 */
  isMobile?: boolean;
}) {
```

然后在**紧接 `return (` 之前**（即所有回调如 `startEdit` / `remove` / `saveDraft` 都已定义之后）插入两个 `const` 箭头函数。位置必须是这里——它们闭包引用 `startEdit` / `remove` / `catalog`，写在使用点之前才不会有 TDZ 问题：

```tsx
  /**
   * 清单的桌面形态：5 列表格。固有宽度约 463px，只在足够宽的窗口里成立。
   */
  const renderCatalogTable = () => (
    <table className="mt-3 w-full table-auto text-left text-sm">
      <thead>
        <tr className="text-xs text-fg-subtle">
          <th className="py-1 pr-2 font-normal">名称</th>
          <th className="py-1 pr-2 font-normal">API 格式</th>
          <th className="py-1 pr-2 font-normal">模型标识</th>
          <th className="py-1 pr-2 font-normal">API 地址</th>
          <th className="py-1 pr-2 font-normal">密钥</th>
          <th className="py-1 font-normal">操作</th>
        </tr>
      </thead>
      <tbody>
        {catalog.models.map((entry) => (
          <tr key={entry.name} className="border-t border-line">
            <td className="py-1.5 pr-2 text-fg">{entry.name}</td>
            <td className="py-1.5 pr-2 text-fg-muted">{entry.provider}</td>
            <td className="py-1.5 pr-2 text-fg-muted">{entry.model}</td>
            <td className="py-1.5 pr-2 font-mono text-xs text-fg-subtle">
              {entry.api_url}
              {suspectsMissingV1(entry) && (
                <span className="ml-2 font-sans text-fg-muted">
                  （OpenAI 兼容接口地址通常以 /v1 结尾，请确认）
                </span>
              )}
            </td>
            {/* key 只以掩码示人：原始值从不经过桌面端。 */}
            <td className="py-1.5 pr-2 font-mono text-xs text-fg-subtle">
              {entry.has_key ? entry.api_key_masked : "未设置"}
            </td>
            <td className="py-1.5">
              <button
                type="button"
                onClick={() => startEdit(entry)}
                className="rounded px-2 py-0.5 text-xs text-fg-muted hover:text-fg"
              >
                编辑
              </button>
              <button
                type="button"
                aria-label={`删除 ${entry.name}`}
                onClick={() => void remove(entry.name)}
                className="ml-1 rounded px-2 py-0.5 text-xs text-fg-muted hover:text-fg"
              >
                删除
              </button>
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );

  /**
   * 清单的手机形态：一条模型一张纵向卡片。
   *
   * 表格固有约 463px 宽，而 390pt 手机上全屏设置页的内宽约 390px——表格怎么调都
   * 装不下，且面板只允许纵向滚动，被裁掉的列（密钥、操作）永久不可达。卡片把
   * 宽度交给容器，任何宽度下所有字段与按钮都在视野内。字段标签在卡片形态下必须
   * 显式渲染（表格形态下由 `<th>` 承担）。
   */
  const renderCatalogCards = () => (
    <ul className="mt-3 space-y-3">
      {catalog.models.map((entry) => (
        <li
          key={entry.name}
          data-model-card
          className="rounded-md border border-line p-3"
        >
          <h3 className="text-sm font-medium text-fg">{entry.name}</h3>
          <dl className="mt-2 space-y-2 text-xs">
            <div>
              <dt className="text-fg-subtle">API 格式</dt>
              <dd className="text-fg-muted">{entry.provider}</dd>
            </div>
            <div>
              <dt className="text-fg-subtle">模型标识</dt>
              <dd className="break-all text-fg-muted">{entry.model}</dd>
            </div>
            <div>
              <dt className="text-fg-subtle">API 地址</dt>
              <dd className="break-all font-mono text-fg-subtle">
                {entry.api_url}
                {suspectsMissingV1(entry) && (
                  <span className="mt-1 block font-sans text-fg-muted">
                    （OpenAI 兼容接口地址通常以 /v1 结尾，请确认）
                  </span>
                )}
              </dd>
            </div>
            <div>
              <dt className="text-fg-subtle">密钥</dt>
              {/* key 只以掩码示人：原始值从不经过桌面端。 */}
              <dd className="font-mono text-fg-subtle">
                {entry.has_key ? entry.api_key_masked : "未设置"}
              </dd>
            </div>
          </dl>
          <div className="mt-3 flex items-center gap-2">
            <button
              type="button"
              onClick={() => startEdit(entry)}
              className="rounded border border-line-strong px-3 py-2 text-sm text-fg-muted hover:text-fg"
            >
              编辑
            </button>
            <button
              type="button"
              aria-label={`删除 ${entry.name}`}
              onClick={() => void remove(entry.name)}
              className="rounded border border-line-strong px-3 py-2 text-sm text-fg-muted hover:text-fg"
            >
              删除
            </button>
          </div>
        </li>
      ))}
    </ul>
  );
```

然后把 `return (` 里那段 `<table className="mt-3 w-full table-auto text-left text-sm"> ... </table>`（从 `<table className="mt-3 w-full table-auto text-left text-sm">` 到它的 `</table>`，即原 `:340-386`）整体替换为：

```tsx
        isMobile ? renderCatalogCards() : renderCatalogTable()
```

同时把上面的 `{catalog.models.length === 0 ? ( ... ) : ( <table .../> )}` 三元里的后半支照此替换，保持结构不变：

```tsx
      ) : (
        isMobile ? renderCatalogCards() : renderCatalogTable()
      )}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/components/SettingsModelsTab.test.tsx
```

Expected: PASS —— 既有 22 条 + 新增 4 条全绿（既有用例都不传 `isMobile`，走表格分支）。

- [ ] **Step 5: 确认类型与构建**

```bash
cd desktop && npx tsc --noEmit
```

Expected: 无错误。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add desktop/src/components/SettingsModelsTab.tsx desktop/src/components/SettingsModelsTab.test.tsx && git commit -m "feat(desktop): 模型设置窄屏改卡片,消除表格横向溢出"
```

---

### Task 3: 输入控件 16px 规则 + 静态门禁补齐

**Files:**
- Modify: `desktop/src/index.css`
- Test: `desktop/src/mobileLayout.test.ts`

**Interfaces:**
- Consumes: Task 1 建立的 `.settings-panel` 挂点类与 `mobileLayout.test.ts` 的 `describe("phone layout CSS")` 块。
- Produces: 无新接口；补齐 Task 1 Step 8 里刻意留到最后的那条 16px 门禁测试。

- [ ] **Step 1: 追加 CSS 规则**

在 `desktop/src/index.css` 末尾（Task 1 那组设置规则的后面）追加：

```css
/*
 * 设置面板内的输入控件字号下限：面板里的输入框是 `text-sm`（14px），而 iOS 在聚焦
 * 字号 < 16px 的输入框时会自动放大页面——`lockZoom` 只能禁掉用户的手动捏合缩放，
 * 挡不住这条自动放大；放大后页面不再贴合屏幕，之后每次滑动都变成两轴平移。
 * 16px 是 iOS 不触发自动放大的下限，与侧栏搜索框那条（见上）同源、同理由。
 * 桌面端不匹配 `[data-mobile]`，逐字节不变。
 */
[data-mobile="true"] .settings-panel input,
[data-mobile="true"] .settings-panel select {
  font-size: 16px;
}
```

- [ ] **Step 2: 补上门禁测试**

在 `desktop/src/mobileLayout.test.ts` 的 `describe("phone layout CSS")` 块末尾追加（即 Task 1 Step 7 留下的那条）：

```ts
  it("keeps the settings inputs at 16px so iOS cannot auto-zoom on focus", () => {
    // 面板里的文本输入框是 14px（`text-sm`），低于 iOS 自动放大阈值 16px。`lockZoom`
    // 只禁手动捏合，挡不住这条自动放大——放大后页面不再贴合屏幕，每次滑动变成两轴
    // 平移。与侧栏搜索框那条同源、同理由。
    const rule =
      /\[data-mobile="true"\]\s+\.settings-panel\s+(?:input|select)[^{]*\{([^}]*)\}/.exec(
        css(),
      )?.[1] ?? "";
    expect(rule).toMatch(/font-size:\s*16px/);
  });
```

- [ ] **Step 3: 跑门禁测试**

```bash
cd desktop && npx vitest run src/mobileLayout.test.ts
```

Expected: PASS —— 全部门禁绿。

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add desktop/src/index.css desktop/src/mobileLayout.test.ts && git commit -m "fix(desktop): 设置面板输入框钉 16px,堵住 iOS 聚焦自动放大"
```

---

### Task 4: 真实 Chrome 几何验证与文档收尾

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/bug-list.md`

**Interfaces:**
- Consumes: Task 1–3 的全部产出。
- Produces: 无代码接口；产出实测证据与文档记录。

- [ ] **Step 1: 起一个临时验证页（用完删除）**

在 worktree 的 `desktop/` 下建 `verify.html` 与 `src/verify.tsx`，渲染真实 `SettingsDialog`（内容见下方），并起 vite：

```bash
cd desktop && cat > verify.html <<'EOF'
<!doctype html>
<html lang="en" data-mobile="true">
  <head><meta charset="UTF-8" /><meta name="viewport" content="width=device-width, initial-scale=1.0, viewport-fit=cover" /><title>verify</title></head>
  <body><div id="root"></div><script type="module" src="/src/verify.tsx"></script></body>
</html>
EOF
```

`src/verify.tsx`：

```tsx
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./index.css";
import { SettingsDialog } from "./components/SettingsDialog";

const modelsResp = {
  models: [
    { name: "claude-sonnet", provider: "anthropic", api_url: "https://api.anthropic.com", model: "claude-sonnet-4-5", has_key: true, api_key_masked: "sk-…9f2c", api_style: "auto", reasoning_effort: "high" },
    { name: "gpt-5-local", provider: "openai", api_url: "https://api.openai.com/v1", model: "gpt-5", has_key: false, api_key_masked: "", api_style: "responses", reasoning_effort: null },
  ],
  default_model: "claude-sonnet",
  subagent_model: "gpt-5-local",
  effective: { source: "catalog", model_ref: "claude-sonnet", provider: "anthropic", api_url: "https://api.anthropic.com", model: "claude-sonnet-4-5", has_key: true, api_key_masked: "sk-…9f2c" },
};
const devicesResp = { devices: [
  { id: "d1", name: "iPhone 15 Pro", scope: "Control", created_at: 1, last_seen_at: 2 },
  { id: "d2", name: "iPad", scope: "Admin", created_at: 1, last_seen_at: 2 },
]};
const call = async (method: string) => {
  if (method === "model/list") return modelsResp;
  if (method === "device/list") return devicesResp;
  if (method === "plugins/list") return { plugins: [{ name: "superpowers-kanban", queryable: true, switch_key: "superpowers-kanban" }] };
  if (method === "plugin/settings/read") return { settings: { default_max_tasks: 2, interval_secs: 60, windows: [
    { days: "Mon-Fri", start: "09:00", end: "18:00", all_day: false, max_tasks: 2 }] } };
  return {};
};
createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <div className="app-shell flex h-full"><div className="app-main flex-1 p-2 text-fg">chat</div></div>
    <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}}
      remoteCall={call} pluginCall={call} modelCall={call}
      pluginProjects={["/Users/me/projects/demo"]} pluginDefaultProject="/Users/me/projects/demo"
      relayUrl="wss://relay.example.com/connect?session=abc123" />
  </StrictMode>
);
```

```bash
cd desktop && npx vite --port 5299 --strictPort > /tmp/vite-verify.log 2>&1 &
sleep 4; curl -s -o /dev/null -w "vite:%{http_code}\n" http://localhost:5299/verify.html
```

- [ ] **Step 2: 用真实 Chrome 在 390/375/320pt 量测四个 Tab**

用 CDP（`--headless=new --remote-debugging-port=9360 --remote-allow-origins=*`，
`Emulation.setDeviceMetricsOverride`）逐个宽度、逐个 Tab 点开后求值：

```js
(() => {
  const p = document.querySelector('[role="tabpanel"]');
  return { clientW: p.clientWidth, scrollW: p.scrollWidth, overflow: p.scrollWidth - p.clientWidth };
})()
```

Expected: **四档宽度 × 四个 Tab 全部 `overflow === 0`**（改动前是 +4 / +28 / +183 / +352）。
同时截图确认：顶部 Tab 横排、内容占满整宽、无遮罩、关闭按钮 44×44。

- [ ] **Step 3: 桌面零回归量测**

同一 CDP 会话里去掉 `data-mobile`（在页面里执行
`document.documentElement.removeAttribute("data-mobile")` 后重新点开各 Tab，或另建一个不带
`data-mobile` 的验证页），在 800×600（桌面最小窗口）与 1280×800 下量测
`.settings-shell` / `.settings-nav` / `.settings-panel` 的 `getBoundingClientRect()`。

Expected: 与改动前的桌面几何**一致**。判据（在 800×600 与 1280×800 下分别量）：

- `dialog.width === min(0.7 * viewportWidth, 768)`——`w-[70vw]` 的 70% 与 `max-w-3xl` 的 768px 上限取小（800px 窗口 → 560px；1280px 窗口 → 768px）。
- `nav.width === 160`——左侧竖栏 `w-40` 未被改动。
- `dialog.height === 0.7 * viewportHeight`——`h-[70vh]`。

这是「桌面未传 `isMobile`、`<html>` 无 `data-mobile`、所有 `[data-mobile="true"]` 规则均不命中」的直接验证。

- [ ] **Step 4: 删除临时验证页**

```bash
cd desktop && rm -f verify.html src/verify.tsx && pkill -f "vite --port 5299"; cd .. && git status --short
```

Expected: 只剩将要提交的文档改动（验证页是未跟踪文件，删掉后不应出现）。

- [ ] **Step 5: 更新文档**

在 `docs/project-management/desktop.md` 的「设置界面」条目旁追加一条（沿用该文件既有的
「— 实现文件；验证命令」格式）：

```markdown
- [x] 手机端设置界面改**全屏页 + 顶部横排 Tab**（原先手机端是 `w-[70vw]` 居中弹窗 + 写死 160px 的左侧竖栏，390pt 上内容面板只剩 111px 且四个 Tab 全部横向溢出滚不到）— `desktop/src/components/SettingsDialog.tsx`（新增 `isMobile` prop、`aria-orientation` 随方向切换、挂点类 `.settings-shell`/`.settings-header`/`.settings-nav`/`.settings-panel`）、`desktop/src/index.css`（`[data-mobile="true"]` 覆写：全屏化、导航横排、隐藏遮罩、顶部/底部安全区、输入控件 16px）、`desktop/src/App.tsx:2329`（传 `isMobile`）、`desktop/src/components/SettingsModelsTab.tsx`（窄屏 5 列表格 → 纵向卡片）；验证 `cd desktop && npx vitest run src/components/SettingsDialog.test.tsx src/components/SettingsModelsTab.test.tsx src/mobileLayout.test.ts` + 真实 Chrome 390/375/320pt 四 Tab 零横向溢出、桌面 800/1280px 几何不变
```

并在 `docs/bug-list.md` 追加修复条目（沿用该文件「根因 → 修复 → 验证」的长句式风格，
末尾附 `— [设计](...)`）：

```markdown
- [x] 手机端设置界面「看起来不太自然、设置内容区域过于小」（根因：**两处固定量的叠加**。弹窗 `w-[70vw] max-w-3xl` 在 390pt 上只有 273px，而左侧 Tab 栏是写死的 `w-40`（160px，不随视口缩放），吃掉 59%，内容面板只剩 111px；四个 Tab 全部横向溢出——实测 通用 +4px / 远程访问 +28px / 插件 +183px（29 元素越界）/ 模型 +352px（30 元素越界，5 列表格固有 463px）。溢出还滚不到：面板是 `overflow-y-auto`，横向溢出既不显示滚动条也滚不到，右侧内容永久不可达，正是 `docs/bug-list.md` 里「看不到每行删除按钮」的同一个坑。另有一处独立缺陷：面板内输入框 14px，低于 iOS 聚焦自动放大的 16px 下限，点一下就整页放大到不贴合屏幕。修复：手机端把设置从居中弹窗改为**全屏页**（`[data-mobile="true"] .settings-shell` 覆写成 `position:fixed; inset:0`）、导航从左侧竖栏改为**顶部横排**（每格 `flex-1`，320/360/375/390/430pt 五档零溢出，最窄档每格 80px）、隐藏遮罩、顶部/底部安全区各补一次（头部栏与唯一的滚动容器）、面板内输入控件钉 16px；「模型」Tab 窄屏把 5 列表格换成纵向卡片（表格固有宽度装不下，与外壳无关）。桌面端全部改动都在 `isMobile` 条件渲染与 `[data-mobile="true"]` 前缀下，逐字节不变 — `desktop/src/components/SettingsDialog.tsx`、`desktop/src/index.css`、`desktop/src/components/SettingsModelsTab.tsx`、`desktop/src/App.tsx`。验证：`cd desktop && npx vitest run src/components/SettingsDialog.test.tsx src/components/SettingsModelsTab.test.tsx src/mobileLayout.test.ts`（新增手机端方向语义、挂点类、卡片/表格二选一、四条静态门禁）、`npx vitest run`（全量）、`npx tsc --noEmit`、`npm run build`；真实 Chrome 实测 390/375/320pt × 四个 Tab 全部 `panel.scrollWidth <= panel.clientWidth`（修复前 111px 面板 vs 463px 内容），桌面 800/1280px 弹窗几何与改动前一致 — [设计](../superpowers/specs/2026-10-17-mobile-settings-layout-design.md)
```

- [ ] **Step 6: 全量验证**

```bash
cd desktop && npx vitest run 2>&1 | tail -20 && npx tsc --noEmit && npm run build 2>&1 | tail -5
```

Expected: 全量测试全绿（当前基线 1047 + 本计划新增约 12 条）、类型无错、构建成功。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add docs/project-management/desktop.md docs/bug-list.md && git commit -m "docs: 记录手机端设置全屏页改造与实测判据"
```

---

### Task 5: 合回 main

**Files:** 无（分支操作）

**Interfaces:**
- Consumes: Task 1–4 的提交。
- Produces: `main` 上的合并提交；分支与 worktree 清理。

- [ ] **Step 1: 确认 worktree 内测试全绿且工作区干净**

```bash
cd .worktrees/mobile-settings && git status --short && cd desktop && npx vitest run 2>&1 | tail -5
```

Expected: 工作区干净、全绿。

- [ ] **Step 2: 合回 main（`--no-ff`，保留分支结构）**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent && git checkout main && git merge --no-ff fix/mobile-settings-layout -m "Merge branch 'fix/mobile-settings-layout'：手机端设置改全屏页 + 顶部 Tab 条"
```

Expected: 无冲突（本分支只碰 `desktop/` 与 `docs/`，且从当前 main 切出）。

- [ ] **Step 3: 在 main 上复跑判据**

```bash
cd desktop && npx vitest run src/components/SettingsDialog.test.tsx src/components/SettingsModelsTab.test.tsx src/mobileLayout.test.ts && npx tsc --noEmit
```

Expected: 全绿。

- [ ] **Step 4: 清理分支与 worktree**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent && git worktree remove .worktrees/mobile-settings && git branch -d fix/mobile-settings-layout && git worktree list
```

Expected: worktree 与分支均已移除，只剩主工作区与其他既有 worktree。