# 状态栏 token 用量翻滚数字 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把状态栏右下角的 token 用量数字改成逐位竖向翻滚（odometer），数字变化时不再瞬间跳变。

**Architecture:** 纯函数 `rollingDigits.ts` 负责拆位、进退位步数、时长与错峰；`RollingNumber.tsx` 用 CSS `transition` 驱动每条数字带的 `translateY`，连续更新时浏览器从当前位置续滚；`StatusBar.tsx` 接线并保留可访问文本；`index.css` 提供裁切窗口的挂点类。

**Tech Stack:** React 19、TypeScript、Tailwind v4（仅少量挂点 CSS）、Vitest + jsdom。

**Spec:** `docs/superpowers/specs/2026-10-19-statusbar-rolling-digits-design.md`

## Global Constraints

- 不动 `UsagePanel` 浮层、不改子 agent 计数、不加千分位。
- 不改服务端用量口径或推送频率。
- 状态栏高度与手机端单行规则（`[data-mobile="true"] .app-statusbar { white-space: nowrap }`）不得回退。
- 系统 `prefers-reduced-motion: reduce` 时必须直接跳到终值。
- commit message 用 conventional commits，首行 <= 72 字符，不写 `Co-Authored-By`。
- 所有代码改动在 worktree `feat/statusbar-rolling-digits` 内完成。

---

### Task 1: 纯函数 `rollingDigits.ts`

**Files:**
- Create: `desktop/src/lib/rollingDigits.ts`
- Test: `desktop/src/lib/rollingDigits.test.ts`

**Interfaces:**
- Produces:
  - `type Direction = -1 | 0 | 1`
  - `STRIP_BASE = 10`、`BASE_MS = 260`、`PER_STEP_MS = 45`、`MAX_MS = 720`、`STAGGER_MS = 20`、`MAX_STAGGER_MS = 120`
  - `splitDigits(value: number): number[]`
  - `digitMove(from: number, to: number, direction: Direction): { steps: number; direction: Direction }`
  - `digitDurationMs(steps: number): number`
  - `digitDelayMs(indexFromRight: number): number`
  - `stripY(pos: number): string`（返回 `translateY(-<pos>em)`）

- [ ] **Step 1: Write the failing test**

```ts
import { describe, expect, it } from "vitest";
import {
  BASE_MS,
  MAX_MS,
  MAX_STAGGER_MS,
  PER_STEP_MS,
  STAGGER_MS,
  digitDelayMs,
  digitDurationMs,
  digitMove,
  splitDigits,
  stripY,
} from "./rollingDigits";

describe("splitDigits", () => {
  it("splits a number into digits, most significant first", () => {
    expect(splitDigits(1234)).toEqual([1, 2, 3, 4]);
  });

  it("renders zero as a single digit", () => {
    expect(splitDigits(0)).toEqual([0]);
  });

  it("ignores sign and fraction", () => {
    expect(splitDigits(-42)).toEqual([4, 2]);
    expect(splitDigits(12.9)).toEqual([1, 2]);
  });
});

describe("digitMove", () => {
  it("moves one step when a digit rolls over upward", () => {
    expect(digitMove(9, 0, 1)).toEqual({ steps: 1, direction: 1 });
  });

  it("walks the long way upward from 0 to 9", () => {
    expect(digitMove(0, 9, 1)).toEqual({ steps: 9, direction: 1 });
  });

  it("mirrors the walk when the value decreases", () => {
    expect(digitMove(0, 9, -1)).toEqual({ steps: 1, direction: -1 });
    expect(digitMove(9, 0, -1)).toEqual({ steps: 9, direction: -1 });
  });

  it("does not move a digit whose face is unchanged", () => {
    expect(digitMove(4, 4, 1)).toEqual({ steps: 0, direction: 0 });
  });

  it("does not move anything when the value is unchanged", () => {
    expect(digitMove(4, 7, 0)).toEqual({ steps: 0, direction: 0 });
  });
});

describe("digitDurationMs", () => {
  it("is zero for no movement", () => {
    expect(digitDurationMs(0)).toBe(0);
  });

  it("grows with the number of steps", () => {
    expect(digitDurationMs(1)).toBe(BASE_MS);
    expect(digitDurationMs(3)).toBe(BASE_MS + 2 * PER_STEP_MS);
  });

  it("caps at MAX_MS", () => {
    expect(digitDurationMs(99)).toBe(MAX_MS);
  });
});

describe("digitDelayMs", () => {
  it("starts the least significant digit immediately", () => {
    expect(digitDelayMs(0)).toBe(0);
    expect(digitDelayMs(1)).toBe(STAGGER_MS);
  });

  it("caps the stagger", () => {
    expect(digitDelayMs(20)).toBe(MAX_STAGGER_MS);
  });
});

describe("stripY", () => {
  it("translates by one line per position", () => {
    expect(stripY(10)).toBe("translateY(-10em)");
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/lib/rollingDigits.test.ts`
Expected: FAIL — `Failed to resolve import "./rollingDigits"`.

- [ ] **Step 3: Write minimal implementation**

```ts
/**
 * 状态栏用量数字的滚动几何。
 *
 * 纯函数，无 DOM：拆位、进退位步数、时长与错峰。组件只负责把这些数字写成
 * 内联样式，因此「滚几格、滚多久、晚多久起步」都能在 jsdom 里直接断言。
 */

/** 数字带里第一份 `0-9` 的起始索引；位置从这里起算，上下各留 9 步余量。 */
export const STRIP_BASE = 10;

/** 滚一步的基础时长与每多一步的增量。 */
export const BASE_MS = 260;
export const PER_STEP_MS = 45;

/** 单次滚动的时长上限。 */
export const MAX_MS = 720;

/** 高位相对个位的起步延迟，及其上限。 */
export const STAGGER_MS = 20;
export const MAX_STAGGER_MS = 120;

export type Direction = -1 | 0 | 1;

/** 把数值拆成数字位，最高位在前；负数取绝对值，小数截断。 */
export function splitDigits(value: number): number[] {
  const magnitude = Math.trunc(Math.abs(value));
  return String(magnitude)
    .split("")
    .map((char) => Number(char));
}

/**
 * 某个数字位从 `from` 滚到 `to` 要走几步、往哪走。
 *
 * 步数按 10 取模到滚动方向：递增时 `9 -> 0` 只走 1 步（进位），`0 -> 9` 走
 * 9 步；递减时镜像。这正是机械里程表的行为——不能为了「最短路径」让 9 倒着
 * 退回 0，那样方向就错了。
 */
export function digitMove(
  from: number,
  to: number,
  direction: Direction,
): { steps: number; direction: Direction } {
  if (direction === 0 || from === to) return { steps: 0, direction: 0 };
  const steps = direction > 0 ? (to - from + 10) % 10 : (from - to + 10) % 10;
  return steps === 0 ? { steps: 0, direction: 0 } : { steps, direction };
}

/** 滚 `steps` 步要多久；0 步不产生过渡。 */
export function digitDurationMs(steps: number): number {
  if (steps <= 0) return 0;
  return Math.min(BASE_MS + (steps - 1) * PER_STEP_MS, MAX_MS);
}

/** 从右数第 `indexFromRight` 位的起步延迟（个位为 0）。 */
export function digitDelayMs(indexFromRight: number): number {
  if (indexFromRight <= 0) return 0;
  return Math.min(indexFromRight * STAGGER_MS, MAX_STAGGER_MS);
}

/** 数字带在位置 `pos` 处的位移。每行一个字符高，故 1 位 = 1em。 */
export function stripY(pos: number): string {
  return `translateY(-${pos}em)`;
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/lib/rollingDigits.test.ts`
Expected: PASS (12 tests).

- [ ] **Step 5: Commit**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/lib/rollingDigits.ts desktop/src/lib/rollingDigits.test.ts
git commit -m "feat(desktop): add rolling-digit geometry helpers"
```

---

### Task 2: `RollingNumber` 组件与裁切窗口样式

**Files:**
- Create: `desktop/src/components/RollingNumber.tsx`
- Modify: `desktop/src/index.css`（文件末尾追加三个挂点类）
- Test: `desktop/src/components/RollingNumber.test.tsx`

**Interfaces:**
- Consumes: Task 1 的 `splitDigits` / `digitMove` / `digitDurationMs` / `digitDelayMs` / `stripY` / `STRIP_BASE`。
- Produces:
  - `RollingNumber({ value, usageKey }: { value: number; usageKey?: string | null })`
  - DOM 契约：根节点 `.rolling-number`（`aria-hidden="true"`）、每位 `.rolling-digit`、每条数字带 `.rolling-strip`（`data-index`、内联 `transform` 与 `transition`）。

- [ ] **Step 1: Write the failing test**

```tsx
/** @vitest-environment jsdom */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { RollingNumber } from "./RollingNumber";
import { STRIP_BASE, stripY } from "../lib/rollingDigits";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function strips(container: HTMLElement): HTMLElement[] {
  return Array.from(container.querySelectorAll<HTMLElement>(".rolling-strip"));
}

describe("RollingNumber", () => {
  it("shows the value immediately on mount, without a transition", () => {
    const { container } = render(<RollingNumber value={1234} />);
    expect(strips(container).map((el) => el.style.transform)).toEqual([
      stripY(STRIP_BASE + 1),
      stripY(STRIP_BASE + 2),
      stripY(STRIP_BASE + 3),
      stripY(STRIP_BASE + 4),
    ]);
    expect(strips(container).every((el) => el.style.transition === "none")).toBe(true);
  });

  it("rolls only the digits whose face changed", () => {
    const { container, rerender } = render(<RollingNumber value={1234} />);
    rerender(<RollingNumber value={1235} />);
    const [thousands, hundreds, tens, units] = strips(container);
    expect(thousands.style.transition).toBe("none");
    expect(hundreds.style.transition).toBe("none");
    expect(tens.style.transition).toBe("none");
    expect(units.style.transform).toBe(stripY(STRIP_BASE + 5));
    expect(units.style.transition).toContain("260ms");
  });

  it("rolls downward when the value decreases", () => {
    const { container, rerender } = render(<RollingNumber value={1234} />);
    rerender(<RollingNumber value={1230} />);
    const units = strips(container)[3];
    // 4 -> 0 going down: four steps, landing four positions lower.
    expect(units.style.transform).toBe(stripY(STRIP_BASE + 0));
    expect(units.style.transition).toContain(`${260 + 3 * 45}ms`);
  });

  it("jumps without a transition when the usage key changes", () => {
    const { container, rerender } = render(<RollingNumber value={1234} usageKey="a" />);
    rerender(<RollingNumber value={7} usageKey="b" />);
    expect(strips(container).every((el) => el.style.transition === "none")).toBe(true);
    expect(strips(container)[0].style.transform).toBe(stripY(STRIP_BASE + 7));
  });

  it("jumps without a transition under prefers-reduced-motion", () => {
    vi.stubGlobal("matchMedia", (query: string) => ({
      matches: query.includes("prefers-reduced-motion"),
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
    }));
    const { container, rerender } = render(<RollingNumber value={1234} />);
    rerender(<RollingNumber value={1239} />);
    expect(strips(container).every((el) => el.style.transition === "none")).toBe(true);
    expect(strips(container)[3].style.transform).toBe(stripY(STRIP_BASE + 9));
  });

  it("adds a new leading digit at its final face without animating it", () => {
    const { container, rerender } = render(<RollingNumber value={99} />);
    rerender(<RollingNumber value={100} />);
    const [hundreds, tens, units] = strips(container);
    expect(hundreds.style.transform).toBe(stripY(STRIP_BASE + 1));
    expect(hundreds.style.transition).toBe("none");
    // 9 -> 0 on an increase is a one-step rollover.
    expect(tens.style.transform).toBe(stripY(STRIP_BASE + 0));
    expect(units.style.transform).toBe(stripY(STRIP_BASE + 0));
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/components/RollingNumber.test.tsx`
Expected: FAIL — `Failed to resolve import "./RollingNumber"`.

- [ ] **Step 3: Write minimal implementation**

```tsx
import { useEffect, useRef, useState } from "react";
import {
  STRIP_BASE,
  type Direction,
  digitDelayMs,
  digitDurationMs,
  digitMove,
  splitDigits,
  stripY,
} from "../lib/rollingDigits";

/** 数字带：三份 0-9，位置从中段起算，上下各留 9 步余量。 */
const STRIP = "0123456789".repeat(3);

const EASING = "cubic-bezier(0.16, 1, 0.3, 1)";

type DigitState = { pos: number; ms: number; delay: number };

function facePos(digit: number): number {
  return STRIP_BASE + digit;
}

function initialState(value: number): DigitState[] {
  return splitDigits(value).map((digit) => ({ pos: facePos(digit), ms: 0, delay: 0 }));
}

function prefersReducedMotion(): boolean {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return false;
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

/**
 * 逐位竖向翻滚的数字。
 *
 * 每位是一条数字带，外面套一行高的裁切窗口。位置作为状态保存，迁移交给 CSS
 * `transition`——新数据在动画途中到达时，浏览器会从当前实际位置继续走向新目标，
 * 因此不会跳回起点重播。
 *
 * `usageKey` 变化（切换会话）时不播动画：那是换成另一份数据，不是同一个数在变。
 */
export function RollingNumber({
  value,
  usageKey,
}: {
  value: number;
  usageKey?: string | null;
}) {
  const prev = useRef({ value, usageKey });
  const [states, setStates] = useState<DigitState[]>(() => initialState(value));
  const digits = splitDigits(value);

  useEffect(() => {
    const previous = prev.current;
    prev.current = { value, usageKey };
    if (previous.value === value && previous.usageKey === usageKey) return;

    const keyChanged = previous.usageKey !== usageKey;
    const direction: Direction = value > previous.value ? 1 : value < previous.value ? -1 : 0;
    const reduced = prefersReducedMotion();
    const previousDigits = splitDigits(previous.value);
    const width = digits.length;

    setStates((old) =>
      digits.map((to, index) => {
        const indexFromRight = width - 1 - index;
        const oldIndex = old.length - 1 - indexFromRight;
        const fromIndex = previousDigits.length - 1 - indexFromRight;
        const from = fromIndex >= 0 ? previousDigits[fromIndex] : 0;
        const current = old[oldIndex];

        if (keyChanged || reduced) {
          return { pos: facePos(to), ms: 0, delay: 0 };
        }
        // 新增的高位：元素刚挂载，直接出现在终值上。
        if (!current) return { pos: facePos(to), ms: 0, delay: 0 };

        const move = digitMove(from, to, direction);
        if (move.steps === 0) return { pos: current.pos, ms: 0, delay: 0 };

        const target = current.pos + move.steps * move.direction;
        // 数字带只有三份：目标越界时先回到同字的中段，再照常滚过去。
        if (target < 0 || target >= STRIP.length) {
          return { pos: facePos(to), ms: 0, delay: 0 };
        }
        return {
          pos: target,
          ms: digitDurationMs(move.steps),
          delay: digitDelayMs(indexFromRight),
        };
      }),
    );
  }, [value, usageKey, digits]);

  return (
    <span className="rolling-number" aria-hidden="true">
      {digits.map((digit, index) => {
        const state = states[index] ?? { pos: facePos(digit), ms: 0, delay: 0 };
        return (
          <span key={index} className="rolling-digit">
            <span
              className="rolling-strip"
              data-index={index}
              style={{
                transform: stripY(state.pos),
                transition:
                  state.ms > 0
                    ? `transform ${state.ms}ms ${EASING} ${state.delay}ms`
                    : "none",
              }}
            >
              {STRIP.split("").map((char, charIndex) => (
                <span key={charIndex}>{char}</span>
              ))}
            </span>
          </span>
        );
      })}
    </span>
  );
}
```

在 `desktop/src/index.css` 末尾追加：

```css
/*
 * 状态栏用量数字的翻滚窗口。
 *
 * `.rolling-digit` 是只露出一行高的裁切窗口，`.rolling-strip` 是竖向数字带；
 * 位移由组件内联的 `transform` 驱动，这里只管几何：1 字符宽、1 字符高，
 * 用 `tabular-nums` 保证等宽，窗口才不会随数字宽度抖动。
 *
 * `line-height: 1` 是硬约束：组件的位移单位是 `em`，一条数字必须正好占 1em，
 * 否则窗口里会露出下一行的边。
 */
.rolling-number {
  display: inline-flex;
  align-items: center;
  font-variant-numeric: tabular-nums;
}

.rolling-digit {
  position: relative;
  display: inline-block;
  width: 1ch;
  height: 1em;
  overflow: hidden;
  vertical-align: middle;
}

.rolling-strip {
  display: flex;
  flex-direction: column;
  will-change: transform;
}

.rolling-strip > span {
  display: block;
  height: 1em;
  line-height: 1em;
  text-align: center;
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/components/RollingNumber.test.tsx`
Expected: PASS (6 tests)。

若 `adds a new leading digit` 一例失败，检查 `old[oldIndex]`：`oldIndex` 对新增高位为负，`old[-1]` 为 `undefined`，应落在「新增高位」分支。

- [ ] **Step 5: Commit**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/components/RollingNumber.tsx desktop/src/components/RollingNumber.test.tsx desktop/src/index.css
git commit -m "feat(desktop): add RollingNumber odometer component"
```

---

### Task 3: 接入 StatusBar 并保留可访问文本

**Files:**
- Modify: `desktop/src/components/StatusBar.tsx:39-51`
- Modify: `desktop/src/App.tsx:2288-2304`（传 `usageId`）
- Test: `desktop/src/components/StatusBar.test.tsx`

**Interfaces:**
- Consumes: Task 2 的 `RollingNumber`。
- Produces: `StatusBar` 新增可选 prop `usageId?: string | null`。

- [ ] **Step 1: Write the failing test**

在 `desktop/src/components/StatusBar.test.tsx` 的 `describe` 内追加：

```tsx
  it("renders the usage as a full sentence for assistive tech", () => {
    render(
      <StatusBar
        cwd="/w"
        status="connected"
        usage={{ model: "m", input: 1234, output: 5678, cacheRead: 0, cacheWrite: 0 }}
        usageId="thread-1"
      />,
    );
    // 可见的数字是翻滚窗口（aria-hidden），完整文本必须另有一份。
    expect(screen.getByText("1234 in / 5678 out")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Token usage details" })).toBeTruthy();
  });

  it("opens the usage panel on click", () => {
    render(
      <StatusBar
        cwd="/w"
        status="connected"
        usage={{ model: "m", input: 1, output: 2, cacheRead: 0, cacheWrite: 0 }}
        usageId="thread-1"
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Token usage details" }));
    expect(screen.getByRole("dialog")).toBeTruthy();
  });
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/components/StatusBar.test.tsx`
Expected: FAIL — 找不到 `1234 in / 5678 out`（当前文本被拆成多个文本节点）。

- [ ] **Step 3: Write minimal implementation**

`StatusBar.tsx`：新增 prop 并替换用量按钮内容。

```tsx
export function StatusBar({
  cwd,
  status,
  usage,
  usageId,
  actions,
  onRepair,
}: {
  cwd: string | null;
  status: string;
  usage: Usage | null;
  /** 当前会话标识：变化时用量数字直接跳到终值，不从别的会话滚过来。 */
  usageId?: string | null;
  actions?: ReactNode;
  onRepair?: () => void;
}) {
```

按钮内容：

```tsx
          <button
            type="button"
            className="inline-flex cursor-pointer items-center gap-1 font-mono hover:text-fg"
            aria-expanded={showUsage}
            aria-controls="usage-panel"
            aria-label="Token usage details"
            onClick={() => setShowUsage((v) => !v)}
          >
            <span className="sr-only">
              {usage.input} in / {usage.output} out
            </span>
            <span aria-hidden="true" className="inline-flex items-center gap-1">
              <RollingNumber value={usage.input} usageKey={usageId} />
              <span>in /</span>
              <RollingNumber value={usage.output} usageKey={usageId} />
              <span>out</span>
            </span>
          </button>
```

文件顶部加 `import { RollingNumber } from "./RollingNumber";`。

`App.tsx` 的状态栏调用加：

```tsx
            usage={current?.session.usage ?? null}
            usageId={sess.currentId}
```

- [ ] **Step 4: Run tests to verify they pass**

Run:
```bash
cd desktop && npx vitest run src/components/StatusBar.test.tsx src/components/RollingNumber.test.tsx src/lib/rollingDigits.test.ts
```
Expected: PASS（全部）。

- [ ] **Step 5: Commit**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/components/StatusBar.tsx desktop/src/components/StatusBar.test.tsx desktop/src/App.tsx
git commit -m "feat(desktop): roll token usage digits in the status bar"
```

---

### Task 4: 全量回归与项目进度登记

**Files:**
- Modify: `docs/project-management/desktop.md`

- [ ] **Step 1: 跑桌面端全量测试与构建**

```bash
cd desktop && npx vitest run && npx tsc --noEmit && npm run build
```
Expected: 全绿，构建成功。

- [ ] **Step 2: 登记进度**

在 `docs/project-management/desktop.md` 的完成清单末尾追加一条：

```markdown
- [x] 状态栏用量数字逐位翻滚（odometer：只滚变化的位、递增递减都正确、连续推送从当前位置续滚、切换会话直接跳终值、尊重 `prefers-reduced-motion`）— `desktop/src/lib/rollingDigits.ts`（拆位 / 步数 / 时长 / 错峰纯函数）、`desktop/src/components/RollingNumber.tsx`（CSS `transition` 驱动数字带 `translateY`）、`desktop/src/components/StatusBar.tsx`（可见翻滚 + `sr-only` 完整文本）、`desktop/src/index.css`（`.rolling-number` / `.rolling-digit` / `.rolling-strip` 裁切窗口）；验证 `cd desktop && npx vitest run src/lib/rollingDigits.test.ts src/components/RollingNumber.test.tsx src/components/StatusBar.test.tsx && npx tsc --noEmit && npm run build`；见 [设计](../superpowers/specs/2026-10-19-statusbar-rolling-digits-design.md)
```

- [ ] **Step 3: Commit**

```bash
git add docs/project-management/desktop.md
git commit -m "docs: record status bar rolling digits"
```

---

## Self-Review

- 设计目标 1–5 分别由 Task 1（几何与方向）、Task 2（动画与重置）、Task 3（接线与无障碍）覆盖。
- 非目标全部未被触碰：`UsagePanel.tsx`、子 agent 计数、服务端用量口径均无改动。
- 类型一致性：`RollingNumber` 的 `usageKey` 与 `StatusBar` 的 `usageId` 在两个任务里同名同义；`stripY` 在 Task 1 定义、Task 2 测试直接引用。