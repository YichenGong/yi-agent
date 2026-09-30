# Tool Card Summary + Progress Narration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the desktop GUI show what each tool call is actually doing, and make the model narrate its progress between tool calls.

**Architecture:** Two independent, loosely-coupled changes. (A) A pure frontend function derives a one-line summary from the already-delivered `item.input` and the collapsed `ToolCallCard` header renders it. (C1) Four lines added to the built-in system prompt in `yi-agent-core` ask the model to narrate in the same response as its tool calls, at least every ~10 calls. No protocol, translator, or agent-loop changes.

**Tech Stack:** Rust (yi-agent-core), TypeScript + React 19 + Vitest + @testing-library/react (desktop/).

## Global Constraints

- **Toolchain PATH (every shell):** this environment does not put cargo or node on PATH. Prefix every command:
  `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/opt/homebrew/bin:$PATH"`.
- **Cargo runs from `yi-agent-rs/`** (the worktree root has no `Cargo.toml`).
- Spec: `docs/superpowers/specs/2026-09-30-tool-call-narration-and-card-summary-design.md`.
- Work in the worktree `.worktrees/feat/tool-card-summary-and-narration` (branch `feat/tool-card-summary-and-narration`). Never commit to `main` directly.
- No protocol change: do NOT add fields to `Item::ToolCall` (`yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`), and do NOT touch `translate.rs`.
- Do NOT modify the existing `You work efficiently by minimizing round-trips. Tool use strategy:` paragraph in `default_system_prompt()`, nor the bash tool description in `yi-agent-rs/crates/yi-agent-tools/src/shell/bash.rs`.
- Rust: run `cargo fmt --all` before every commit. No `Co-Authored-By` lines in commit messages. Conventional-commit prefixes.
- Frontend tests: `cd desktop && npx vitest run <file>`; typecheck with `npx tsc --noEmit`.
- After each task that changes behavior, sync `docs/project-management/` and `docs/bug-list.md` in the SAME commit (project rule).
- Rust test commands must run from the worktree root (never two cargo commands concurrently).

---

### Task 1: Progress-narration section in the default system prompt

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:135-184` (`AgentConfig::default_system_prompt`)
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:3530-3548` (prompt assertions, `mod tests`)
- Modify: `docs/project-management/yi-agent-core.md` (Features)
- Modify: `docs/bug-list.md` (mark the "bash 块之间没有讲解" entry done)

**Interfaces:**
- Consumes: nothing.
- Produces: `AgentConfig::default_system_prompt() -> String` now contains a `Progress narration:` block. Later tasks do not depend on it.

- [ ] **Step 1: Write the failing test**

Add next to the existing `default_system_prompt_contains_identity_and_strategy` test in `agent.rs`:

```rust
    #[test]
    fn default_system_prompt_requires_progress_narration() {
        let prompt = AgentConfig::default_system_prompt();
        assert!(
            prompt.contains("Progress narration:"),
            "prompt must name the narration section so a reviewer can find it"
        );
        assert!(
            prompt.contains("1-2 sentences"),
            "prompt must ask for a short prose lead-in on tool-calling responses"
        );
        assert!(
            prompt.contains("At least every ~10 tool calls"),
            "prompt must bound how long the agent may stay silent"
        );
        assert!(
            prompt.contains("Narration rides along with the tool call in the same response"),
            "narration must not be readable as a reason to split tool calls"
        );
        assert!(
            prompt.contains("in the language the user writes in"),
            "narration must follow the user's language"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::default_system_prompt_requires_progress_narration -- --exact`
Expected: FAIL — `prompt must name the narration section so a reviewer can find it`.

- [ ] **Step 3: Add the prompt section**

In `default_system_prompt()`, insert this block immediately after the `Style: Never use emoji ...` line and before the blank line that precedes `Task execution:`:

```
Progress narration:
- Say what you are doing while you do it: a response that issues tool calls
  opens with 1-2 sentences of prose saying what you are about to do and why.
- Never let a long run of tool calls go silent. At least every ~10 tool calls,
  stop and narrate where you are, what you found so far, and what is next;
  narrating more often is fine.
- Narration rides along with the tool call in the same response. It never means
  splitting one tool call into several.
- Narrate in the language the user writes in.
```

The result must read, in order: identity paragraph, tool-use strategy paragraph, `Style:`, `Progress narration:`, `Task execution:`, `File discovery:`, `Subagent integration:`.

- [ ] **Step 4: Run the prompt tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::default_system_prompt`
Expected: PASS — `default_system_prompt_requires_progress_narration`, `default_system_prompt_contains_identity_and_strategy` (still contains `minimizing round-trips` and `MULTIPLE tool calls`), `default_system_prompt_discourages_unbounded_glob`.

- [ ] **Step 5: Run the crate and the two prompt-asserting call sites**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib`
Expected: all pass.

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent`
Expected: all pass (`yi-agent/src/main.rs` asserts on this same prompt: `default_system_prompt_requires_a_workdir_for_isolation`).

- [ ] **Step 6: Sync project docs**

In `docs/project-management/yi-agent-core.md`, add a Features bullet after the batch-tool-call one:

```markdown
- [x] 工具调用进度叙述提示词 — `crates/yi-agent-core/src/agent.rs::default_system_prompt()` 的 `Progress narration:` 段：每条带工具调用的响应先说 1-2 句在做什么，至少每约 10 次工具调用汇报一次，明确叙述不意味着拆分调用；验证：`cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::default_system_prompt_requires_progress_narration -- --exact`
```

In `docs/bug-list.md`, append a done entry:

```markdown
- [x] 桌面端大量 bash 块之间没有文字讲解（根因：`default_system_prompt()` 只约束"最小化往返 / 合并 bash"，没有任何叙述要求，模型于是长期静默；实测 55.3% 的 tool 往返无文本块，app-server 会话第 1 步达 69%）。修复：新增 `Progress narration:` 段（四要点：默认 1-2 句导语 / 至少每约 10 次工具调用汇报 / 叙述不增加往返 / 跟随用户语言）。见 [设计](../superpowers/specs/2026-09-30-tool-call-narration-and-card-summary-design.md)。验证：`cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::default_system_prompt_requires_progress_narration`。注：纯提示词方案，属软约束
```

- [ ] **Step 7: Format and commit**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/tool-card-summary-and-narration/yi-agent-rs && cargo fmt --all
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/tool-card-summary-and-narration
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs docs/project-management/yi-agent-core.md docs/bug-list.md
git commit -m "feat(prompt): ask the agent to narrate progress between tool calls"
```

---

### Task 2: `toolCallSummary` pure function

**Files:**
- Create: `desktop/src/lib/toolSummary.ts`
- Test: `desktop/src/lib/toolSummary.test.ts`

**Interfaces:**
- Consumes: nothing.
- Produces: `export function toolCallSummary(name: string, input: unknown): string | null` — Task 3 imports this exact name and signature.

- [ ] **Step 1: Write the failing test**

Create `desktop/src/lib/toolSummary.test.ts`:

```ts
import { describe, it, expect } from "vitest";
import { toolCallSummary } from "./toolSummary";

describe("toolCallSummary", () => {
  it("uses the command for bash and process_start", () => {
    expect(toolCallSummary("bash", { command: "git status" })).toBe("git status");
    expect(toolCallSummary("process_start", { command: "npm run dev" })).toBe("npm run dev");
  });

  it("uses the path for file tools", () => {
    expect(toolCallSummary("read", { path: "src/App.tsx" })).toBe("src/App.tsx");
    expect(toolCallSummary("write", { path: "docs/x.md" })).toBe("docs/x.md");
    expect(toolCallSummary("edit", { path: "a.rs" })).toBe("a.rs");
    expect(toolCallSummary("view_image", { path: "/tmp/a.png" })).toBe("/tmp/a.png");
  });

  it("uses the pattern for grep and glob", () => {
    expect(toolCallSummary("grep", { pattern: "rename", path: "desktop" })).toBe("rename");
    expect(toolCallSummary("glob", { pattern: "**/*.tsx" })).toBe("**/*.tsx");
  });

  it("uses the path for the Skill tool", () => {
    expect(toolCallSummary("Skill", { path: "/home/u/.yi-agent/skills/x/SKILL.md" })).toBe(
      "/home/u/.yi-agent/skills/x/SKILL.md",
    );
  });

  it("uses the query and url for web tools", () => {
    expect(toolCallSummary("web_search", { query: "rust lifetimes" })).toBe("rust lifetimes");
    expect(toolCallSummary("web_fetch", { url: "https://example.com" })).toBe(
      "https://example.com",
    );
  });

  it("returns null when the field is absent", () => {
    expect(toolCallSummary("bash", {})).toBeNull();
    expect(toolCallSummary("read", { offset: 1 })).toBeNull();
    expect(toolCallSummary("bash", { command: 42 })).toBeNull();
    expect(toolCallSummary("bash", { command: "" })).toBeNull();
    expect(toolCallSummary("bash", { command: "   " })).toBeNull();
  });

  it("returns null for unknown tools and non-object input", () => {
    expect(toolCallSummary("mystery", { command: "x" })).toBeNull();
    expect(toolCallSummary("bash", null)).toBeNull();
    expect(toolCallSummary("bash", "git status")).toBeNull();
    expect(toolCallSummary("bash", 42)).toBeNull();
    expect(toolCallSummary("bash", ["git status"])).toBeNull();
  });

  it("collapses whitespace and keeps only the first line", () => {
    expect(toolCallSummary("bash", { command: "mkdir -p a\ncd a\ntouch b" })).toBe("mkdir -p a");
    expect(toolCallSummary("bash", { command: "  ls   -la  " })).toBe("ls -la");
  });

  it("truncates to 80 columns with an ellipsis", () => {
    const long = "x".repeat(200);
    const out = toolCallSummary("bash", { command: long });
    expect(out).not.toBeNull();
    expect(out!.endsWith("...")).toBe(true);
    expect(out!.length).toBeLessThanOrEqual(80);
    const exact = "y".repeat(80);
    expect(toolCallSummary("bash", { command: exact })).toBe(exact);
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/lib/toolSummary.test.ts`
Expected: FAIL — cannot resolve `./toolSummary`.

- [ ] **Step 3: Write the implementation**

Create `desktop/src/lib/toolSummary.ts`:

```ts
/**
 * One-line summary of what a tool call is doing, for the collapsed card header.
 *
 * Mirrors the TUI's field choice (`yi-agent-rs/crates/yi-agent/src/tui/history.rs`
 * `permission_summary`): bash-like tools show the command, file tools show the
 * path. Anything unexpected returns null and the header falls back to the bare
 * tool name — we never stringify an arbitrary payload into the title.
 */

/** Tools whose most informative argument is `command`. */
const COMMAND_TOOLS = new Set(["bash", "process_start"]);
/** Tools whose most informative argument is `path`. */
const PATH_TOOLS = new Set(["read", "write", "edit", "view_image", "Skill"]);
/** Tools whose most informative argument is `pattern`. */
const PATTERN_TOOLS = new Set(["grep", "glob"]);
/** Tools whose most informative argument is `url`. */
const URL_TOOLS = new Set(["web_fetch"]);

const WEB_SEARCH = "web_search";
const MAX_COLUMNS = 80;

function pick(input: unknown, key: string): string | null {
  if (typeof input !== "object" || input === null || Array.isArray(input)) return null;
  const value = (input as Record<string, unknown>)[key];
  if (typeof value !== "string") return null;
  // First line only, with runs of whitespace collapsed: a multi-line heredoc
  // would otherwise render as mojibake in a single-line header.
  const oneLine = value.split("\n")[0].replace(/\s+/g, " ").trim();
  return oneLine === "" ? null : oneLine;
}

function truncate(text: string): string {
  if (text.length <= MAX_COLUMNS) return text;
  return `${text.slice(0, MAX_COLUMNS - 3)}...`;
}

export function toolCallSummary(name: string, input: unknown): string | null {
  let raw: string | null = null;
  if (COMMAND_TOOLS.has(name)) raw = pick(input, "command");
  else if (PATH_TOOLS.has(name)) raw = pick(input, "path");
  else if (PATTERN_TOOLS.has(name)) raw = pick(input, "pattern");
  else if (URL_TOOLS.has(name)) raw = pick(input, "url");
  else if (name === WEB_SEARCH) raw = pick(input, "query");
  return raw === null ? null : truncate(raw);
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/lib/toolSummary.test.ts`
Expected: PASS (9 tests).

- [ ] **Step 5: Typecheck and commit**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/tool-card-summary-and-narration/desktop && npx tsc --noEmit
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/tool-card-summary-and-narration
git add desktop/src/lib/toolSummary.ts desktop/src/lib/toolSummary.test.ts
git commit -m "feat(desktop): derive a one-line summary from tool call input"
```

---

### Task 3: Render the summary in the card header

**Files:**
- Modify: `desktop/src/components/ToolCallCard.tsx:1-32`
- Test: `desktop/src/components/ToolCallCard.test.tsx` (create)
- Modify: `docs/project-management/desktop.md` (Features + 验证命令 counts)
- Modify: `docs/bug-list.md`

**Interfaces:**
- Consumes: `toolCallSummary(name: string, input: unknown): string | null` from `desktop/src/lib/toolSummary.ts` (Task 2).
- Produces: nothing other tasks depend on.

- [ ] **Step 1: Write the failing test**

Create `desktop/src/components/ToolCallCard.test.tsx`:

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import { ToolCallCard } from "./ToolCallCard";
import type { Item } from "../lib/protocol";

type ToolCallItem = Extract<Item, { type: "toolCall" }>;

afterEach(cleanup);

const bash = (input: unknown, over: Partial<ToolCallItem> = {}): ToolCallItem => ({
  type: "toolCall",
  id: "i1",
  call_id: "c1",
  name: "bash",
  input,
  status: "completed",
  ...over,
});

describe("ToolCallCard", () => {
  it("shows the command in the collapsed header", () => {
    render(<ToolCallCard item={bash({ command: "git status --short" })} />);
    expect(screen.getByText("git status --short")).toBeTruthy();
  });

  it("shows the path for a read", () => {
    render(<ToolCallCard item={bash({ path: "src/App.tsx" }, { name: "read" })} />);
    expect(screen.getByText("src/App.tsx")).toBeTruthy();
  });

  it("falls back to the bare tool name for an unknown tool", () => {
    render(<ToolCallCard item={bash(42, { name: "mystery" })} />);
    expect(screen.getByText("mystery")).toBeTruthy();
  });

  it("still renders the full input JSON when expanded", () => {
    const { container } = render(<ToolCallCard item={bash({ command: "ls -la" })} />);
    fireEvent.click(screen.getByRole("button"));
    expect(container.textContent).toContain('"command": "ls -la"');
  });

  it("labels the summary for hover and screen readers", () => {
    render(<ToolCallCard item={bash({ command: "ls -la" })} />);
    expect(screen.getByTitle("ls -la")).toBeTruthy();
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/components/ToolCallCard.test.tsx`
Expected: FAIL — the first three cases cannot find the command / path text (header renders only `bash`).

- [ ] **Step 3: Wire the summary into the header**

In `desktop/src/components/ToolCallCard.tsx`: add the import, compute the summary, and render it between the name and the status pill.

```tsx
import { useId, useState } from "react";
import type { Item, ToolStatus } from "../lib/protocol";
import { toolCallSummary } from "../lib/toolSummary";

type ToolCallItem = Extract<Item, { type: "toolCall" }>;
```

Inside the component, before the `return`:

```tsx
  const summary = toolCallSummary(item.name, item.input);
```

Replace the header's name span:

```tsx
        <span className="font-mono font-medium text-neutral-200">{item.name}</span>
        {summary && (
          <span className="min-w-0 truncate font-mono text-neutral-400" title={summary}>
            {summary}
          </span>
        )}
```

Leave the status pill (`ml-auto`) and the whole collapsible body untouched.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/components/ToolCallCard.test.tsx`
Expected: PASS (5 tests).

- [ ] **Step 5: Run the whole frontend suite**

Run: `cd desktop && npx vitest run && npx tsc --noEmit`
Expected: all pass (baseline is 144 existing; + 9 from Task 2 + 5 from this task = 158).

- [ ] **Step 6: Sync project docs**

In `docs/project-management/desktop.md`, add a Features bullet after the tool-card line:

```markdown
- [x] 工具卡片折叠态显示"在做什么"摘要（bash/process_start 取 `command`，read/write/edit/view_image/Skill 取 `path`，grep/glob 取 `pattern`，web_search 取 `query`，web_fetch 取 `url`；取不到只显示工具名，绝不把任意 payload 塞进标题）— `desktop/src/lib/toolSummary.ts:43`（`toolCallSummary`）/ `desktop/src/components/ToolCallCard.tsx:27`（头部摘要 + `title` 悬停）；验证 `cd desktop && npx vitest run src/lib/toolSummary.test.ts src/components/ToolCallCard.test.tsx`
```

Update the `**验证命令：**` line's counts: `142 个前端单测` → `158 个前端单测`（实际基线为 144：`App.test.tsx` 已增至 15）, append ` + desktop/src/lib/toolSummary.test.ts 9` before ` + desktop/src/components/MarkdownText.test.tsx 5`, and append ` + desktop/src/components/ToolCallCard.test.tsx 5` after the `MarkdownText` entry.

In `docs/bug-list.md`, append a done entry:

```markdown
- [x] 桌面端 bash 工具卡片上只有 `bash` 几个字母，看不出在做什么（根因：`ToolCallCard.tsx` 头部只渲染 `item.name`，`item.input` 虽然随 `item/started` 一起下发，却只放在默认折叠的 Input 区；TUI 早已是 `name(input)` + 语义摘要，桌面端从未接上）。修复：新增 `desktop/src/lib/toolSummary.ts` 纯函数按工具名取 `command`/`path`/`pattern`/`query`/`url`，压成单行、80 列截断，取不到返回 `null` 退化为只显示工具名；`ToolCallCard` 头部渲染该摘要并加 `title` 悬停。见 [设计](../superpowers/specs/2026-09-30-tool-call-narration-and-card-summary-design.md)。验证：`cd desktop && npx vitest run src/lib/toolSummary.test.ts src/components/ToolCallCard.test.tsx`
```

- [ ] **Step 7: Commit**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/tool-card-summary-and-narration
git add desktop/src/components/ToolCallCard.tsx desktop/src/components/ToolCallCard.test.tsx docs/project-management/desktop.md docs/bug-list.md
git commit -m "feat(desktop): show what a tool call is doing in the card header"
```

---

### Task 4: Full-suite gate

**Files:** none (verification only).

**Interfaces:**
- Consumes: everything from Tasks 1-3.
- Produces: a green tree ready to merge.

- [ ] **Step 1: Confirm no stray cargo processes**

Run: `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent" || echo "clean"`
Expected: `clean` (or only this agent's own processes). Kill any leftovers before continuing — a stale test binary holds the target lock and later runs hang or die with exit 137.

- [ ] **Step 2: Full Rust gate**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib`
Expected: all pass.

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent`
Expected: all pass.

Run: `cargo test -p yi-agent-app-server`
Expected: all pass (untouched, must not regress).

Run: `cargo fmt --all --check`
Expected: no diff.

- [ ] **Step 3: Full frontend gate**

Run: `cd desktop && npx vitest run`
Expected: 158 passed.

Run: `cd desktop && npx tsc --noEmit`
Expected: no errors.

Run: `cd desktop && npm run build`
Expected: build succeeds.

- [ ] **Step 4: Confirm the tree is clean**

Run: `git status --short`
Expected: no output.

---

## Self-Review

**Spec coverage:** C1 prompt section → Task 1 (all four bullets asserted individually). A card summary → Tasks 2-3 (field table covered per tool, `null` fallback covered, truncation covered, collapsed header covered). Non-goals honored: no protocol field, no translator change, no agent-loop injection, no reasoning display. Doc sync → Tasks 1 and 3. Verification list from spec §9 → Task 4.

**Placeholder scan:** no TODO/TBD; every step has runnable commands and full code.

**Type consistency:** `toolCallSummary(name: string, input: unknown): string | null` is defined in Task 2 and consumed with the same name/signature in Task 3. `ToolCallItem` in the Task 3 test matches the `Extract<Item, { type: "toolCall" }>` alias already in `ToolCallCard.tsx`.
