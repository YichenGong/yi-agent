# Phone Floating Sidebar Toggle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** On the phone client, replace the full-height 44px menu rail on the left with a single floating menu button, giving the chat column its full width back.

**Architecture:** The toggle button moves out of the in-flow `flex-row` rail and becomes an absolutely-positioned overlay inside the main column (`.app-main`). The obsolete rail CSS padding rule is deleted and replaced by a `.app-main` top padding the button's height so content is never covered. All selectors stay gated on `[data-mobile="true"]`, so the desktop layout is byte-for-byte unchanged.

**Tech Stack:** React 19 + TypeScript, Tailwind CSS v4 (semantic tokens), Vite, Vitest + @testing-library/react.

## Global Constraints

- **Desktop unchanged:** every behavioral/CSS change is gated so the desktop build renders byte-for-byte identically. The button renders only under `isMobile`; every CSS rule added/kept to drive the phone layout is prefixed with `[data-mobile="true"]`.
- **No new color tokens:** the button background uses the existing `--panel` token (`bg-panel/80`).
- **Preserve the accessible name:** the toggle keeps `aria-label="会话列表"` and `aria-expanded={sidebarOpen}` — existing tests select it by that label.
- **Preserve the 44px touch target** (Apple HIG minimum) and the existing hamburger `<svg>` path.
- **Removal of `app-mobile-toggle` is intended:** after this change the class is unused, so its CSS rule is deleted (it also double-counts the safe-area inset already applied by `[data-mobile="true"] body`).

---

## File Structure

- `desktop/src/App.tsx` — remove the in-flow toggle rail (lines ~1724-1738); render the same button as an overlay inside the main column; tag the main column with `app-main`.
- `desktop/src/index.css` — delete the `.app-mobile-toggle { padding-top: … }` rule (~lines 243-246); add `[data-mobile="true"] .app-main { padding-top: 48px }`.
- `desktop/src/App.test.tsx` — one new mobile test asserting the toggle is a `.app-main` overlay (NOT a rail).
- `desktop/src/mobileLayout.test.ts` — one new static gate asserting the real `index.css` has the `.app-main` padding and no longer has the obsolete `.app-mobile-toggle` padding.

---

### Task 1: Float the toggle inside the main column (App.tsx)

**Files:**
- Modify: `desktop/src/App.tsx:1723-1738` (remove rail), `desktop/src/App.tsx:1786` (add `app-main` + overlay button)
- Test: `desktop/src/App.test.tsx:2202-2243` (new `it` in the existing `describe("App mobile layout (phone client)")`)

**Interfaces:**
- Consumes: `isMobile: boolean` and `sidebarOpen` / `setSidebarOpen` already in scope in `App`; the existing hamburger `<svg>`.
- Produces: a DOM node `[aria-label="会话列表"]` whose closest ancestor carries class `app-main` and whose own className contains `absolute`. Task 2's CSS rule keys off `.app-main`.

- [ ] **Step 1: Write the failing test**

Insert this `it` block inside the existing `describe("App mobile layout (phone client)", () => { … })`, right after the `"leaves the desktop layout untouched"` test (before the closing `});` that currently sits at `App.test.tsx:2243`):

```tsx
  it("floats the toggle over the chat column instead of a fixed rail", async () => {
    asRemote();
    render(<App />);
    const toggle = await screen.findByLabelText("会话列表");
    // 浮在主列内（.app-main），而不是一条 in-flow、通栏高的竖条列。
    expect(toggle.closest(".app-main")).not.toBeNull();
    expect(toggle.className).toContain("absolute");
  });
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && ./node_modules/.bin/vitest run src/App.test.tsx -t "floats the toggle"`

Expected: FAIL — `toggle.closest(".app-main")` is `null` and/or `toggle.className` has no `absolute` (today the button's class is `app-mobile-toggle flex w-11 shrink-0 items-start …` and it lives in the outer `flex-row`, not in `.app-main`).

- [ ] **Step 3: Remove the in-flow rail and render the button as an overlay**

In `desktop/src/App.tsx`, **delete** this whole `{isMobile && ( … )}` button block (currently lines ~1724-1738, the first child of the `<div className="flex min-h-0 flex-1 flex-row">`):

```tsx
        {isMobile && (
          <button
            type="button"
            aria-label="会话列表"
            aria-expanded={sidebarOpen}
            onClick={() => setSidebarOpen(true)}
            // 手机上的唯一会话入口：32px 的细条在触屏上既难点到也不好发现，放宽到
            // 44px（Apple HIG 的最小触控目标）并放大图标。
            className="app-mobile-toggle flex w-11 shrink-0 items-start justify-center border-r border-line bg-panel pt-2 text-fg-muted hover:text-fg"
          >
            <svg viewBox="0 0 16 16" className="size-5" fill="currentColor" aria-hidden="true">
              <path d="M2 4h12v1.5H2V4Zm0 3.25h12v1.5H2v-1.5ZM2 10.5h12V12H2v-1.5Z" />
            </svg>
          </button>
        )}
```

Then change the main-column opening tag (currently `App.tsx:1786`):

from

```tsx
        <div className="relative flex min-w-0 flex-1 flex-col">
```

to

```tsx
        <div className="app-main relative flex min-w-0 flex-1 flex-col">
          {isMobile && (
            <button
              type="button"
              aria-label="会话列表"
              aria-expanded={sidebarOpen}
              onClick={() => setSidebarOpen(true)}
              // 手机上的唯一会话入口：44px 触控目标（Apple HIG），浮在主列左上角，
              // 不再占一条常驻的左侧竖条——抽屉本身是 fixed 离屏的，收起时聊天区
              // 因此能拿满整宽。顶部让位内边距由 .app-main 提供（见 index.css），
              // 免得按钮压住首条消息或看板顶栏。
              className="absolute left-1 top-1 z-10 flex size-11 items-center justify-center rounded-md bg-panel/80 text-fg-muted hover:text-fg"
            >
              <svg viewBox="0 0 16 16" className="size-5" fill="currentColor" aria-hidden="true">
                <path d="M2 4h12v1.5H2V4Zm0 3.25h12v1.5H2v-1.5ZM2 10.5h12V12H2v-1.5Z" />
              </svg>
            </button>
          )}
```

(The `{isMobile && (…)}` overlay is now the first child of the main column; the existing `{selectedBoard !== null ? … : …}` expression stays directly after it, unchanged.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && ./node_modules/.bin/vitest run src/App.test.tsx -t "mobile layout"`

Expected: PASS — all 4 tests in the mobile-layout describe (the 3 pre-existing plus the new one) are green.

- [ ] **Step 5: Commit**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): float the phone drawer toggle over the chat column"
```

---

### Task 2: Give the floating button its space, drop the obsolete rail padding (index.css + static gate)

**Files:**
- Modify: `desktop/src/index.css:243-246` (delete `.app-mobile-toggle` rule); add `[data-mobile="true"] .app-main` rule inside the mobile group (near `index.css:202-205`)
- Test: `desktop/src/mobileLayout.test.ts` (new `it` in `describe("phone layout CSS")`)

**Interfaces:**
- Consumes: the `.app-main` class added to the main column in Task 1.
- Produces: nothing downstream; this is the last task.

- [ ] **Step 1: Write the failing test**

Insert this `it` inside the existing `describe("phone layout CSS", () => { … })` in `desktop/src/mobileLayout.test.ts` (e.g. right after the `"hides the desktop-only titlebar strip on the phone"` test):

```ts
  it("floats the drawer toggle over the main column instead of a rail", () => {
    // 收起抽屉时聊天列本该拿满整宽；那颗按钮原先坐在一条常驻的 44px 竖条列里，白占
    // 横向宽度。改为浮层后，让位交给主列自己的顶部内边距：按钮 top(4px) + 高 44px，
    // 内容从 48px 起，首条消息与看板顶栏都不会被压住。
    expect(css()).toMatch(/\[data-mobile="true"\]\s+\.app-main\s*\{[^}]*padding-top:\s*48px/);
    // 竖条按钮那条补白规则必须消失：`body` 已经加了同一段安全区，两处叠加会把图标
    // 下推两倍——按钮已不再是那条竖条。
    expect(css()).not.toMatch(/\.app-mobile-toggle\s*\{[^}]*padding-top/);
  });
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && ./node_modules/.bin/vitest run src/mobileLayout.test.ts -t "floats the drawer toggle"`

Expected: FAIL — the `[data-mobile="true"] .app-main { … padding-top: 48px … }` rule does not exist yet, and `.app-mobile-toggle { padding-top: … }` still does.

- [ ] **Step 3: Edit index.css**

**Delete** the obsolete rule (currently `index.css:243-246`):

```css
/* 手机端的抽屉开关按钮；桌面端不渲染该节点，这里只处理安全区留白。 */
.app-mobile-toggle {
  padding-top: env(safe-area-inset-top, 0px);
}
```

**Add** this rule immediately after the existing `[data-mobile="true"] .app-sidebar aside { … }` block (currently ending at `index.css:205`):

```css
/*
 * 浮在主列左上角那颗抽屉开关的让位：按钮 top(4px) + 高 44px，内容因此从 48px 起，
 * 首条消息与看板顶栏都不会被压住。桌面端不匹配该选择器，逐字节不变。
 */
[data-mobile="true"] .app-main {
  padding-top: 48px;
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd desktop && ./node_modules/.bin/vitest run src/mobileLayout.test.ts`

Expected: PASS — all tests in the file (the 11 pre-existing plus the new one) are green.

Then run the full frontend suite to confirm nothing regressed:

Run: `cd desktop && ./node_modules/.bin/vitest run`

Expected: PASS — the whole `desktop` suite is green.

- [ ] **Step 5: Commit**

```bash
git add desktop/src/index.css desktop/src/mobileLayout.test.ts
git commit -m "fix(desktop): reclaim the phone chat width; pad the main column for the floating toggle"
```

---

## Self-Review

**Spec coverage:**
- Spec §3.1.1 remove in-flow rail → Task 1 Step 3. ✔
- Spec §3.1.2 float the button inside the main column, keeping attrs/icon → Task 1 Step 3. ✔
- Spec §3.1.3 add `app-main` class → Task 1 Step 3. ✔
- Spec §3.2.1 delete `.app-mobile-toggle` padding rule (fixes double safe-area) → Task 2 Step 3 + gate assertion. ✔
- Spec §3.2.2 add `[data-mobile="true"] .app-main { padding-top: 48px }` → Task 2 Step 3 + gate assertion. ✔
- Spec §3.3 App.test.tsx untouched-but-green + new mobileLayout gate → Task 1 Step 4 / Task 2 Steps 1,4. ✔
- Spec §4 edge cases (board header covered by the 48px pad; `z-10` under modals; landscape) → addressed by the single `.app-main` padding + `z-10`; no extra tasks needed. ✔
- Spec §5 "don't touch drawer/longpress/etc." → no task touches them. ✔

**Placeholder scan:** No "TBD"/"TODO"/"handle edge cases"/"similar to Task N"; every code step shows the exact code. ✔

**Type consistency:** The class name `app-main` is spelled identically in Task 1 (JSX), Task 2 (CSS rule + regex gate). The accessible name `会话列表` matches the existing tests' selector. The `size-11` / `top-1` / `48px` values are internally consistent: button occupies y=4..48px, content starts at y=48px. ✔

**Note for the implementer (single spec ambiguity resolved):** the button keeps the decided `bg-panel/80` background plus `rounded-md`. If a future review asks for a fully transparent button, drop only `bg-panel/80`; nothing else changes.
