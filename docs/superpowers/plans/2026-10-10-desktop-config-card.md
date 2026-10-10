# Desktop `/config` Config Card Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render the desktop `/config` command output as a structured card (title + grouped key/value rows) instead of a single grey text capsule.

**Architecture:** A new presentational component `ConfigCard` owns the card layout and value humanisation. `Session` gains an optional structured payload on its `notice` items (`noticeConfig`), and `ChatView`'s `notice` branch renders `ConfigCard` when that payload is present, falling back to the existing capsule otherwise. `App`'s `/config` case switches from emitting a text blob to emitting the structured payload. No protocol item type is added; the card is composed entirely client-side.

**Tech Stack:** React 19, TypeScript, Tailwind v4 (semantic tokens), Vitest + @testing-library/react (jsdom).

## Global Constraints

- No new protocol item type. The card rides on the existing `notice` item via an optional `config` field. Copied from spec §3.3.
- Card border/background must use semantic tokens (`border-line-strong`, `bg-surface`), never literal colour scales. Copied from spec §3.1.
- Field labels are Chinese; the three group titles are exactly `模型与服务`, `运行环境`, `限额`. Copied from spec §2.
- `sandbox` value maps to kebab-case wire names `read-only` / `workspace-write` / `danger-full-access` (matching `yi-agent-tools::SandboxMode::as_str`), falling back to the raw string for unknown modes. Copied from spec §3.2.
- Booleans render as `是` / `否`; an unset `api_key` renders `未设置`, a present one `已设置（***）`; any missing field renders `—`. Copied from spec §3.2 / §3.1.
- All verification runs from `desktop/`: `npx tsc --noEmit` and `npx vitest run`.

---

## File Structure

- Create `desktop/src/components/ConfigCard.tsx` — the presentational card + value humanisation + the static group/label table. One responsibility: turn a `config/read` result (or an error) into a card.
- Create `desktop/src/components/ConfigCard.test.tsx` — its unit tests.
- Modify `desktop/src/lib/session.ts` — add the optional `config` payload to `NoticeItem` and the `noticeConfig` method.
- Modify `desktop/src/lib/session.test.ts` — cover the new payload.
- Modify `desktop/src/components/ChatView.tsx` — the `notice` branch renders `ConfigCard` when a payload is present.
- Modify `desktop/src/components/ChatView.test.tsx` — cover both notice shapes.
- Modify `desktop/src/App.tsx` — the `/config` case emits the structured payload.
- Modify `desktop/src/App.test.tsx` — cover the happy and failure paths end-to-end.

---

### Task 1: `ConfigCard` component

**Files:**
- Create: `desktop/src/components/ConfigCard.tsx`
- Test: `desktop/src/components/ConfigCard.test.tsx`

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces:
  - `export function ConfigCard({ config, error }: { config?: Record<string, unknown> | null; error?: string | null })`
  - The card root carries `data-testid="config-card"` and the heading text `当前配置`.

- [ ] **Step 1: Write the failing test**

Create `desktop/src/components/ConfigCard.test.tsx`:

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { ConfigCard } from "./ConfigCard";

afterEach(cleanup);

// A full `config/read` `redacted_view()` as the server sends it.
const sample: Record<string, unknown> = {
  provider: "anthropic",
  api_url: "https://api.anthropic.com",
  api_key: "***",
  model: "claude-sonnet-4",
  max_turns: 20,
  max_resident_subagents: 8,
  max_direct_children: 4,
  workdir: "/Users/me/a/very/long/project/path",
  sandbox: "WorkspaceWrite",
  sandbox_promotable: true,
  yolo: false,
  compact_threshold: 160000,
};

describe("ConfigCard", () => {
  it("titles the card and shows the three groups", () => {
    render(<ConfigCard config={sample} />);
    expect(screen.getByTestId("config-card")).toBeTruthy();
    expect(screen.getByText("当前配置")).toBeTruthy();
    expect(screen.getByText("模型与服务")).toBeTruthy();
    expect(screen.getByText("运行环境")).toBeTruthy();
    expect(screen.getByText("限额")).toBeTruthy();
  });

  it("renders the sandbox mode as its kebab-case wire name", () => {
    render(<ConfigCard config={sample} />);
    expect(screen.getByText("workspace-write")).toBeTruthy();
    expect(screen.queryByText("WorkspaceWrite")).toBeNull();
  });

  it("maps every known sandbox mode", () => {
    for (const [raw, label] of [
      ["ReadOnly", "read-only"],
      ["WorkspaceWrite", "workspace-write"],
      ["DangerFullAccess", "danger-full-access"],
    ] as const) {
      const { unmount } = render(<ConfigCard config={{ ...sample, sandbox: raw }} />);
      expect(screen.getByText(label)).toBeTruthy();
      unmount();
    }
  });

  it("renders booleans as 是 / 否", () => {
    render(<ConfigCard config={sample} />);
    expect(screen.getByText("是")).toBeTruthy(); // sandbox_promotable
    expect(screen.getByText("否")).toBeTruthy(); // yolo
  });

  it("labels an unset api key honestly", () => {
    render(<ConfigCard config={{ ...sample, api_key: "" }} />);
    expect(screen.getByText("未设置")).toBeTruthy();
  });

  it("shows 已设置（***） when a key is present", () => {
    render(<ConfigCard config={sample} />);
    expect(screen.getByText("已设置（***）")).toBeTruthy();
  });

  it("shows — for a field the response omits", () => {
    // 12 known rows; with only `provider` present the other 11 are dashes.
    render(<ConfigCard config={{ provider: "anthropic" }} />);
    expect(screen.getAllByText("—")).toHaveLength(11);
  });

  it("ignores keys it does not know", () => {
    render(<ConfigCard config={{ ...sample, mystery: "leak" }} />);
    expect(screen.queryByText("mystery")).toBeNull();
    expect(screen.queryByText("leak")).toBeNull();
  });

  it("wraps a long value instead of breaking the card", () => {
    render(<ConfigCard config={sample} />);
    const path = screen.getByText("/Users/me/a/very/long/project/path");
    expect(path.className).toContain("break-all");
    expect(path.className).toContain("min-w-0");
  });

  it("shows a read failure in the same card", () => {
    render(<ConfigCard error="boom" />);
    expect(screen.getByTestId("config-card")).toBeTruthy();
    expect(screen.getByText("当前配置")).toBeTruthy();
    expect(screen.getByText("读取配置失败: boom")).toBeTruthy();
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/components/ConfigCard.test.tsx`
Expected: FAIL — `Failed to resolve import "./ConfigCard"`.

- [ ] **Step 3: Write minimal implementation**

Create `desktop/src/components/ConfigCard.tsx`:

```tsx
/**
 * `/config` 的输出卡片。
 *
 * `config/read` 的 `redacted_view()` 是一组机器字段：`sandbox` 是 Rust 的 `{:?}`
 * 名、布尔是 `true/false`、密钥是掩码。这里把它们翻译成给人看的一行行键值，
 * 按「模型与服务 / 运行环境 / 限额」三组排进一张卡片——取代此前挤成一段等宽
 * 文本的灰色 notice。
 */

/** 布尔 → 中文；非布尔（缺字段）落回破折号。 */
function yesNo(v: unknown): string {
  return v === true ? "是" : v === false ? "否" : EM_DASH;
}

/**
 * 沙箱模式：Rust `{:?}` 名 → 线协议的 kebab 名。
 *
 * 取值与 `yi-agent-tools::SandboxMode::as_str` 一致；查不到就用原串，未来新增模式
 * 时不丢信息。
 */
const SANDBOX_LABELS: Record<string, string> = {
  ReadOnly: "read-only",
  WorkspaceWrite: "workspace-write",
  DangerFullAccess: "danger-full-access",
};

function sandboxLabel(v: unknown): string {
  if (typeof v === "string" && v in SANDBOX_LABELS) return SANDBOX_LABELS[v];
  return scalar(v);
}

/**
 * 密钥状态。只说「有没有」——原文从不经过桌面端（服务端已脱敏成 `""` / `"***"`）。
 * 与 `SettingsModelsTab.effectiveKeyLabel` 同精神：这是状态行，不是凭证面板。
 */
function apiKeyLabel(v: unknown): string {
  if (v === undefined || v === null) return EM_DASH;
  return v === "" ? "未设置" : "已设置（***）";
}

/** 缺字段（`undefined` / `null`）显示破折号，其余原样 `String()`。 */
function scalar(v: unknown): string {
  return v === undefined || v === null ? EM_DASH : String(v);
}

const EM_DASH = "—";

type Row = { label: string; value: (cfg: Record<string, unknown>) => string };
type Group = { title: string; rows: Row[] };

/**
 * 卡片呈现的全部字段。静态表即契约：未知键被忽略，所以配置结构演进时不会把
 * 机器键名摊到界面上。
 */
const GROUPS: Group[] = [
  {
    title: "模型与服务",
    rows: [
      { label: "Provider", value: (c) => scalar(c.provider) },
      { label: "API URL", value: (c) => scalar(c.api_url) },
      { label: "API Key", value: (c) => apiKeyLabel(c.api_key) },
      { label: "模型", value: (c) => scalar(c.model) },
    ],
  },
  {
    title: "运行环境",
    rows: [
      { label: "工作目录", value: (c) => scalar(c.workdir) },
      { label: "沙箱", value: (c) => sandboxLabel(c.sandbox) },
      { label: "沙箱可提升", value: (c) => yesNo(c.sandbox_promotable) },
      { label: "YOLO", value: (c) => yesNo(c.yolo) },
    ],
  },
  {
    title: "限额",
    rows: [
      { label: "最大轮次", value: (c) => scalar(c.max_turns) },
      { label: "常驻子代理", value: (c) => scalar(c.max_resident_subagents) },
      { label: "直接子代理", value: (c) => scalar(c.max_direct_children) },
      { label: "压缩阈值", value: (c) => scalar(c.compact_threshold) },
    ],
  },
];

export function ConfigCard({
  config,
  error,
}: {
  config?: Record<string, unknown> | null;
  error?: string | null;
}) {
  return (
    <div
      data-testid="config-card"
      className="my-2 self-stretch rounded-lg border border-line-strong bg-surface px-3 py-2 text-sm"
    >
      <div className="mb-1 font-medium text-fg">当前配置</div>
      {error != null ? (
        <div className="text-red-300">{`读取配置失败: ${error}`}</div>
      ) : (
        <div className="flex flex-col gap-2">
          {GROUPS.map((group) => (
            <div key={group.title} className="flex flex-col gap-0.5">
              <div className="text-xs text-fg-subtle">{group.title}</div>
              {group.rows.map((row) => (
                <div key={row.label} className="flex gap-3">
                  <span className="w-24 shrink-0 text-fg-muted">{row.label}</span>
                  <span className="min-w-0 break-all text-fg">
                    {row.value(config ?? {})}
                  </span>
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

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/components/ConfigCard.test.tsx`
Expected: PASS (10 tests).

- [ ] **Step 5: Commit**

```bash
cd desktop && git add src/components/ConfigCard.tsx src/components/ConfigCard.test.tsx
git commit -m "feat(desktop): add the /config output card component"
```

---

### Task 2: `Session.noticeConfig` payload

**Files:**
- Modify: `desktop/src/lib/session.ts` (the `NoticeItem` interface, and a new method next to `notice`)
- Test: `desktop/src/lib/session.test.ts` (the `notices` describe block)

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces:
  - `NoticeItem.config?: { data?: Record<string, unknown> | null; error?: string | null }`
  - `Session.prototype.noticeConfig(data: Record<string, unknown> | null, error?: string | null): void` — pushes a `notice` item with `text: ""` and a populated `config` payload.

- [ ] **Step 1: Write the failing test**

In `desktop/src/lib/session.test.ts`, inside the existing `describe("notices", …)` block, add these two cases after the `"drops notices on reset"` case:

```ts
    it("carries a structured config payload on a notice", () => {
      const s = new Session();
      s.noticeConfig({ model: "m" });
      const item = s.items[0] as {
        type: string;
        text: string;
        config?: { data?: unknown; error?: unknown };
      };
      expect(item.type).toBe("notice");
      expect(item.text).toBe("");
      expect(item.config?.data).toEqual({ model: "m" });
      expect(item.config?.error).toBeNull();
    });

    it("carries a config read error", () => {
      const s = new Session();
      s.noticeConfig(null, "boom");
      const item = s.items[0] as { config?: { data?: unknown; error?: unknown } };
      expect(item.config?.data).toBeNull();
      expect(item.config?.error).toBe("boom");
    });
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/lib/session.test.ts`
Expected: FAIL — `s.noticeConfig is not a function`.

- [ ] **Step 3: Write minimal implementation**

In `desktop/src/lib/session.ts`, extend `NoticeItem`:

```ts
export interface NoticeItem {
  type: "notice";
  id: string;
  text: string;
  /**
   * 结构化载荷。`/config` 用它携带 `config/read` 的原始结果（或读取错误），
   * 由 `ChatView` 渲染成一张配置卡片。缺省即普通文本 notice。
   */
  config?: { data?: Record<string, unknown> | null; error?: string | null };
}
```

Then add the method right after the existing `notice(text: string)` method:

```ts
  /**
   * Append a command-output notice that carries structured data — `/config`
   * hands over the raw `config/read` result (or its error) for `ChatView` to
   * render as a card. `text` stays empty: the card is the presentation, and no
   * other consumer reads it.
   */
  noticeConfig(data: Record<string, unknown> | null, error?: string | null): void {
    this.items.push({
      type: "notice",
      id: nextLocalId(),
      text: "",
      config: { data, error: error ?? null },
    });
  }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/lib/session.test.ts`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd desktop && git add src/lib/session.ts src/lib/session.test.ts
git commit -m "feat(desktop): let a session notice carry a structured config payload"
```

---

### Task 3: `ChatView` renders the card for a config notice

**Files:**
- Modify: `desktop/src/components/ChatView.tsx` (the `notice` case in `ChatItem`, and the import list)
- Test: `desktop/src/components/ChatView.test.tsx`

**Interfaces:**
- Consumes: `ConfigCard` from Task 1 (`{ config?: Record<string, unknown> | null; error?: string | null }`); `NoticeItem.config` from Task 2.
- Produces: nothing new — `ChatView` keeps its existing props.

- [ ] **Step 1: Write the failing test**

In `desktop/src/components/ChatView.test.tsx`, add a new `describe` after the existing suites:

```tsx
describe("ChatView notices", () => {
  it("renders a config notice as a card", () => {
    const list = [
      {
        type: "notice",
        id: "n1",
        text: "",
        config: { data: { model: "claude-sonnet-4", sandbox: "WorkspaceWrite" } },
      },
    ] as unknown as (Item | NoticeItem)[];
    render(<ChatView items={list} />);
    expect(screen.getByTestId("config-card")).toBeTruthy();
    expect(screen.getByText("当前配置")).toBeTruthy();
    expect(screen.getByText("workspace-write")).toBeTruthy();
  });

  it("renders a plain notice as the compact capsule", () => {
    const list = [{ type: "notice", id: "n2", text: "对话已清空" }] as unknown as (
      | Item
      | NoticeItem
    )[];
    render(<ChatView items={list} />);
    expect(screen.getByText("对话已清空")).toBeTruthy();
    expect(screen.queryByTestId("config-card")).toBeNull();
  });
});
```

Add the type import near the existing `import type { Item } from "../lib/protocol";` line:

```tsx
import type { NoticeItem } from "../lib/session";
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx`
Expected: FAIL — `Unable to find an element by: [data-testid="config-card"]`.

- [ ] **Step 3: Write minimal implementation**

In `desktop/src/components/ChatView.tsx`, add the import:

```tsx
import { ConfigCard } from "./ConfigCard";
```

Replace the `notice` case in `ChatItem`:

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

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd desktop && git add src/components/ChatView.tsx src/components/ChatView.test.tsx
git commit -m "feat(desktop): render a config notice as a card in the chat view"
```

---

### Task 4: `/config` emits the structured payload

**Files:**
- Modify: `desktop/src/App.tsx` (the `case "config":` branch of `onSlashCommand`, around line 1295)
- Test: `desktop/src/App.test.tsx` (inside `describe("App slash commands", …)`)

**Interfaces:**
- Consumes: `Session.noticeConfig(data, error?)` from Task 2; the card rendering from Task 3.
- Produces: nothing new — `/config` still fires `config/read` and never `turn/start`.

- [ ] **Step 1: Write the failing test**

In `desktop/src/App.test.tsx`, inside `describe("App slash commands", …)` (after the `/help` case), add:

```tsx
  it("renders /config as a card without touching the agent", async () => {
    state.dataSources["config/read"] = () => ({
      provider: "anthropic",
      model: "claude-sonnet-4",
      sandbox: "WorkspaceWrite",
    });
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/config" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await screen.findByTestId("config-card");
    expect(screen.getByText("当前配置")).toBeTruthy();
    expect(screen.getByText("workspace-write")).toBeTruthy();
    expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(false);
  });

  it("shows a failed /config read inside the card", async () => {
    state.rejectCode["config/read"] = -1;
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/config" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await screen.findByTestId("config-card");
    expect(screen.getByText(/读取配置失败/)).toBeTruthy();
  });
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "/config"`
Expected: FAIL — no `config-card` test id (the text-blob notice has no such element).

- [ ] **Step 3: Write minimal implementation**

In `desktop/src/App.tsx`, replace the `case "config":` body in `onSlashCommand`:

```ts
      case "config":
        try {
          const cfg = await c.request<Record<string, unknown>>("config/read", {});
          session.noticeConfig(cfg);
        } catch (e) {
          session.noticeConfig(null, formatError(e));
        }
        break;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "/config"`
Expected: PASS (both cases).

- [ ] **Step 5: Run the full suite and typecheck**

Run: `cd desktop && npx tsc --noEmit && npx vitest run`
Expected: PASS — no type errors, all suites green (including the untouched `/help` and `/cost` capsule cases).

- [ ] **Step 6: Commit**

```bash
cd desktop && git add src/App.tsx src/App.test.tsx
git commit -m "feat(desktop): show /config as a card"
```

---

## Self-Review

**1. Spec coverage:**
- Spec §2 (grouped key/value card, inline in the transcript) → Task 1 (component) + Task 4 (App emits it inline). ✔
- Spec §3.1 (card structure, semantic tokens, `data-testid`, `w-24`/`min-w-0`/`break-all`, missing field `—`, unknown keys ignored, error row) → Task 1. ✔
- Spec §3.2 (sandbox mapping, `是/否`, api_key states) → Task 1. ✔
- Spec §3.3 (`NoticeItem.config` optional payload, `noticeConfig`, `ChatView` branch, other commands unchanged) → Task 2 + Task 3. ✔
- Spec §3.4 (no server change, no new protocol type, no collapse/close) → respected by Tasks 1–4 (no `yi-agent-rs` edits; card rides on `notice`). ✔
- Spec §4 (test list) → Tasks 1–4 tests: title+groups, sandbox mapping, booleans, api_key states, missing `—`, unknown keys, `break-all`, error, `noticeConfig` payload, ChatView both shapes, App happy + failure, no `turn/start`. ✔
- Spec §5/§6 (verification + manual checklist) → Task 4 Step 5 runs the full suite + tsc; the manual checklist is a human gate, not automatable. ✔

**2. Placeholder scan:** No `TBD`/`TODO`/"add error handling". Every code step shows full code. ✔

**3. Type consistency:** `ConfigCard`'s prop names (`config`, `error`) match its call site in Task 3 (`config={item.config.data} error={item.config.error}`). `NoticeItem.config` shape `{ data?, error? }` is identical in Task 2 (producer) and Task 3 (consumer). `noticeConfig(data, error?)` signature is identical in Task 2 (definition) and Task 4 (call). The `EM_DASH` const is declared before its first use in the emitted file (hoisting-safe `const` after the functions is fine since the functions only run at render, but the plan lists the helpers first — matching source order in Step 3 is required). ✔

**Note for the implementer:** In Task 1 Step 3 the helper functions reference `EM_DASH` and `SANDBOX_LABELS`, both declared later in the snippet. Write the file exactly as shown — the declarations execute at module load before any render, so `const` ordering is safe; reordering is unnecessary.
