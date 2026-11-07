/**
 * Browser-only fixture for the subagent trace panel.
 *
 * Renders the **real** `ThreadDetailPanel` → `SubagentTrace` → `TraceTimeline` →
 * `TraceEvent` → `TraceComposer` chain with the project's own `index.css`, so the
 * geometry the check script measures is the geometry users get. Nothing here
 * touches a sidecar: rows are built the way the daemon serialises them and go
 * through the real `foldTraceRows`, so a decoding regression shows up as a
 * visual difference rather than being papered over by hand-built blocks.
 *
 * The fixture deliberately owns no listeners and no timers of its own: every
 * scenario change is driven synchronously by `window.traceProbe`, which keeps the
 * measurements deterministic (no "append at 60Hz and hope").
 */
import { useMemo, useState } from "react";
import { flushSync } from "react-dom";
import { createRoot } from "react-dom/client";

import { ThreadDetailPanel } from "../../src/components/ThreadDetailPanel";
import { applyFontScale, type FontScale } from "../../src/lib/fontScale";
import { applyTheme, type Theme } from "../../src/lib/theme";
import type { AgentTraceRow, ProcessInfo } from "../../src/lib/protocol";
import type { TraceActionSnapshot } from "../../src/lib/subagentTraceActions";
import type { TraceSubscriptionSnapshot } from "../../src/lib/subagentTraceSubscription";
import type { SubagentRow } from "../../src/lib/subagents";
import "../../src/index.css";

type Scenario = "long" | "emptyMetadata" | "error" | "children" | "images";

const THREAD = "thread-1";

/** One trace row exactly as `IpcTraceRow` serialises it. */
function row(eventId: number, kind: string, payload: unknown, taskId = "task-a"): AgentTraceRow {
  return { eventId, taskId, kind, payloadJson: JSON.stringify(payload) };
}

const LONG_CHINESE =
  "先确认会话恢复的入口，再把鉴权失败的分支和刷新令牌的竞态一起看一遍。" +
  "这里刻意写一段很长的中文，用来验证窄栏里正文是否会自然折行而不是把整栏撑开，" +
  "也用来验证暂停阅读位置时追加同块文本不会把视口带走。";

const LONG_TOKEN =
  "https://example.com/a/very/long/path/without/any/spaces/" +
  "that/should/wrap/instead/of/overflowing/the/panel/" +
  "9f8e7d6c5b4a39281706f5e4d3c2b1a0";

const NO_SPACE_PATH =
  "/Users/someone/Documents/projects/personal-projects/yi-agent/.worktrees/" +
  "desktop-subagent-trace-redesign/yi-agent-rs/crates/yi-agent-app-server/src/very_long_module_name.rs";

const MARKDOWN = [
  "## 观察",
  "",
  "1. 会话恢复走 `auth/session.ts`；",
  "2. 刷新令牌的竞态在并发请求里复现。",
  "",
  "| 场景 | 结果 | 备注 |",
  "| --- | --- | --- |",
  "| 冷启动 | 正常恢复 | 无重试 |",
  "| 并发刷新 | 重复刷新 | 需要串行化 |",
  "",
  "```ts",
  "export async function refresh(token: string) {",
  "  return fetch(`${LONG_TOKEN}`)",
  "}",
  "```",
  "",
  `长 URL 也要能折行：${LONG_TOKEN}`,
  "",
  `无空格路径也要能折行：${NO_SPACE_PATH}`,
].join("\n");

const LONG_SUMMARY = Array.from({ length: 8 }, (_, i) => `第 ${i + 1} 行摘要：读取成功，包含若干细节`).join("\n");

const LONG_TOOL_SUCCESS = Array.from(
  { length: 12 },
  (_, i) => `line ${i + 1}: ${"x".repeat(40)}`,
).join("\n");

function longRows(): AgentTraceRow[] {
  const rows: AgentTraceRow[] = [];
  let id = 1;
  rows.push(row(id++, "assistant_text", { text: LONG_CHINESE }));
  rows.push(row(id++, "tool_call", { name: "read", summary: "读取文件 src/auth/session.ts" }));
  rows.push(row(id++, "tool_result", { name: "read", summary: LONG_SUMMARY, is_error: false }));
  rows.push(row(id++, "assistant_text", { text: MARKDOWN }));
  rows.push(row(id++, "tool_call", { name: "bash", summary: `执行命令 ${NO_SPACE_PATH}` }));
  rows.push(row(id++, "tool_result", { name: "bash", summary: LONG_TOOL_SUCCESS, is_error: false }));
  rows.push(row(id++, "state_note", { note: "等待授权：写入 auth/session.ts" }));
  rows.push(row(id++, "tool_result", { name: "bash", summary: "", is_error: true }));
  rows.push(row(id++, "assistant_text", { text: "继续检查刷新链路。" }));
  return rows;
}

/** What a newer daemon might write that this client cannot decode. */
function malformedRows(): AgentTraceRow[] {
  return [
    row(100, "assistant_text", { text: "可解析的正文。" }),
    { eventId: 101, taskId: "task-a", kind: "assistant_text", payloadJson: "{not json" },
    row(102, "assistant_text", ["array", "not", "object"]),
    row(103, "tool_call", { name: "read" }),
    row(104, "tool_result", { name: "read", summary: "ok", is_error: "false" }),
    row(105, "assistant_text", { type: "tool_call", text: "标签不符" }),
    row(106, "thinking_block", { text: "未知 kind" }),
  ];
}

function errorRows(): AgentTraceRow[] {
  return [
    row(1, "assistant_text", { text: "失败前已经写下的历史必须保留。" }),
    row(2, "tool_result", { name: "bash", summary: "command not found: cargo", is_error: true }),
  ];
}

function childrenRows(): AgentTraceRow[] {
  return [
    row(1, "assistant_text", { text: "拆成两个子任务并行验证。" }),
    row(2, "state_note", { note: "已派发 2 个子任务" }),
  ];
}

function imageRows(): AgentTraceRow[] {
  return [
    row(1, "assistant_text", { text: `![图表](data:image/svg+xml;charset=utf-8,${encodeURIComponent(
      '<svg xmlns="http://www.w3.org/2000/svg" width="640" height="240"><rect width="640" height="240" fill="#334155"/></svg>',
    )})` }),
    row(2, "assistant_text", { text: "图片之后的正文。" }),
    row(3, "tool_call", { name: "read", summary: "读取文件 src/very/deep/path/with/long/name.ts" }),
  ];
}

function rowsFor(scenario: Scenario): AgentTraceRow[] {
  switch (scenario) {
    case "long":
      return longRows();
    case "emptyMetadata":
      return malformedRows();
    case "error":
      return errorRows();
    case "children":
      return childrenRows();
    case "images":
      return imageRows();
  }
}

function rowFor(scenario: Scenario, overrides: Partial<SubagentRow> = {}): SubagentRow {
  const base: SubagentRow = {
    taskId: "task-a",
    objective: "检查登录流程",
    state: "running",
    lastStep: "正在读取 auth/session.ts",
    finished: false,
    parentTaskId: null,
  };
  if (scenario === "emptyMetadata") {
    // The real "server sent nothing" shape: `objective`/`lastStep` absent.
    return { ...base, objective: null, lastStep: null, ...overrides };
  }
  if (scenario === "error") {
    return { ...base, state: "failed", finished: true, lastStep: null, ...overrides };
  }
  if (scenario === "children") {
    return { ...base, state: "waiting_for_children", ...overrides };
  }
  return { ...base, ...overrides };
}

function childrenFor(scenario: Scenario): SubagentRow[] {
  if (scenario !== "children") return [];
  return [
    {
      taskId: "task-a-1",
      objective: "验证 token 刷新",
      state: "running",
      lastStep: "运行集成测试",
      finished: false,
      parentTaskId: "task-a",
    },
    {
      taskId: "task-a-2",
      objective: "覆盖并发刷新场景",
      state: "completed",
      lastStep: null,
      finished: true,
      parentTaskId: "task-a",
    },
  ];
}

const EMPTY_ACTIONS: TraceActionSnapshot = {
  draft: "",
  draftRevision: 0,
  messagePending: false,
  previewPending: false,
  confirmPending: false,
  confirmation: null,
  error: null,
  notice: null,
};

const PROCESSES: ProcessInfo[] = [];

interface State {
  scenario: Scenario;
  rows: AgentTraceRow[];
  phase: TraceSubscriptionSnapshot["phase"];
  error: string | null;
  disconnected: boolean;
  actions: TraceActionSnapshot;
  selected: string | null;
  /** `ThreadDetailPanel` reads its width from storage at mount: bump to re-read. */
  panelKey: number;
  /**
   * Which view the detail is showing.
   *
   * The timeline's 200-block window is keyed by identity, and a *refresh* of the
   * same task deliberately keeps it. Opening a different history therefore has to
   * be a different identity — which is what opening another task is.
   */
  viewKey: number;
}

let state: State = {
  scenario: "long",
  rows: rowsFor("long"),
  phase: "ready",
  error: null,
  disconnected: false,
  actions: EMPTY_ACTIONS,
  selected: "task-a",
  panelKey: 0,
  viewKey: 0,
};

/** A re-render trigger the probe can call synchronously. */
let rerender: () => void = () => {};

function subscriptionOf(current: State): TraceSubscriptionSnapshot {
  return {
    owner: null,
    phase: current.phase,
    rows: current.rows,
    error: current.error,
    disconnected: current.disconnected,
    waitingForPriorRequest: false,
  };
}

function Fixture() {
  const [, force] = useState(0);
  rerender = () => force((v) => v + 1);

  const rows = state.rows;
  const scenario = state.scenario;
  const selected = state.selected;

  // The panel mirrors the real app: the rail lists tasks, the detail replaces it.
  const railRows = useMemo(() => [rowFor(scenario), ...childrenFor(scenario)], [scenario]);

  const subscription = subscriptionOf(state);

  return (
    <div style={{ display: "flex", height: "100vh", alignItems: "stretch" }}>
      <div style={{ flex: 1, minWidth: 0 }} />
      <ThreadDetailPanel
        key={state.panelKey}
        tab="subagents"
        onTabChange={() => {}}
        onClose={() => {}}
        railRows={railRows}
        selectedTaskId={selected}
        onOpenTask={(taskId) => mutate({ selected: taskId })}
        onLeaveTask={() => mutate({ selected: null })}
        traceProps={
          selected === null
            ? null
            : {
                identityKey: `fixture:${selected}:${state.viewKey}`,
                taskId: selected,
                row: selected === "task-a" ? rowFor(scenario) : (childrenFor(scenario).find((r) => r.taskId === selected) ?? rowFor(scenario, { taskId: selected })),
                parent: null,
                children: selected === "task-a" ? childrenFor(scenario) : [],
                subscription: {
                  ...subscription,
                  rows: selected === "task-a" ? rows : rows.map((r) => ({ ...r, taskId: selected })),
                },
                actions: state.actions,
                onDrill: (taskId: string) => mutate({ selected: taskId }),
                onBack: () => mutate({ selected: null }),
                onRetry: () => {},
                onDraftChange: (draft: string) =>
                  mutate({ actions: { ...state.actions, draft, draftRevision: state.actions.draftRevision + 1 } }),
                onSend: async () => {},
                onPreview: async () => {},
                onConfirm: async () => {},
                // 放弃只撤销本地确认，不发 cancel——与服务端行为一致。
                onDismiss: () => mutate({ actions: { ...state.actions, confirmation: null } }),
              }
        }
        diffProps={{
          diff: null,
          loading: false,
          error: null,
          activeCommit: null,
          onRefresh: () => {},
          onOpenCommit: () => {},
          onCloseCommit: () => {},
          note: null,
        }}
        processProps={{
          processes: PROCESSES,
          selectedProcessId: null,
          onOpenProcess: () => {},
          onCloseProcess: () => {},
          output: null,
          onKill: async () => {},
        }}
        isMobile={false}
      />
    </div>
  );
}

/** Every mutation re-renders synchronously so a measurement sees the settled DOM. */
function mutate(patch: Partial<State>): void {
  state = { ...state, ...patch };
  flushSync(() => rerender());
}

function loadScenario(name: Scenario): void {
  mutate({
    scenario: name,
    rows: rowsFor(name),
    phase: "ready",
    error: null,
    disconnected: false,
    actions: EMPTY_ACTIONS,
    selected: "task-a",
  });
}

/** The one vertical scroller of the timeline, or null when it is not mounted. */
function viewport(): HTMLElement | null {
  return document.querySelector<HTMLElement>("[data-trace-viewport]");
}

function firstVisibleAnchor(): { key: string | null; offset: number } {
  const el = viewport();
  if (!el) return { key: null, offset: 0 };
  const top = el.getBoundingClientRect().top;
  for (const node of el.querySelectorAll<HTMLElement>("[data-trace-key]")) {
    const rect = node.getBoundingClientRect();
    if (rect.bottom <= top) continue;
    return { key: node.dataset.traceKey ?? null, offset: rect.top - top };
  }
  return { key: null, offset: 0 };
}

/** True when some ancestor is its own horizontal scroller (code blocks, tables). */
function insideHorizontalScroller(el: HTMLElement, panel: HTMLElement): boolean {
  let node: HTMLElement | null = el.parentElement;
  while (node !== null && node !== panel) {
    const overflowX = getComputedStyle(node).overflowX;
    if (overflowX === "auto" || overflowX === "scroll") return true;
    node = node.parentElement;
  }
  return false;
}

/**
 * The worst horizontal overflow of the panel, in CSS pixels.
 *
 * Two kinds of element are excluded on purpose, because neither is a layout bug:
 *
 * * anything inside a horizontal scroller — a fenced code block or a wide table is
 *   *supposed* to scroll inside its own box;
 * * visually hidden helpers (`sr-only`), which are 1px wide by design and would
 *   otherwise report their whole nowrap label as overflow.
 *
 * Returns the largest of (a) a scroller's own `scrollWidth - clientWidth` and
 * (b) any element's box sticking out past the panel's edges.
 */
function horizontalOverflow(): number {
  const panel = document.querySelector<HTMLElement>('aside[aria-label="会话详情"]');
  if (!panel) return 0;
  const panelRect = panel.getBoundingClientRect();
  let worst = 0;
  for (const node of panel.querySelectorAll<HTMLElement>("*")) {
    const style = getComputedStyle(node);
    if (style.display === "none" || style.visibility === "hidden") continue;
    const rect = node.getBoundingClientRect();
    if (rect.width <= 1 || rect.height <= 1) continue;
    if (insideHorizontalScroller(node, panel)) continue;
    const overflowX = style.overflowX;
    if (overflowX === "auto" || overflowX === "scroll") {
      worst = Math.max(worst, node.scrollWidth - node.clientWidth);
      continue;
    }
    worst = Math.max(worst, rect.right - panelRect.right, panelRect.left - rect.left);
  }
  return Math.max(0, Math.round(worst));
}

function isVisible(el: Element | null): boolean {
  if (el === null) return false;
  const rect = el.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) return false;
  if (rect.top < -0.5 || rect.bottom > window.innerHeight + 0.5) return false;
  let node: Element | null = el;
  while (node !== null) {
    const style = getComputedStyle(node);
    if (style.visibility === "hidden" || style.display === "none") return false;
    if (node.getAttribute("inert") !== null || node.getAttribute("aria-hidden") === "true") return false;
    node = node.parentElement;
  }
  return document.elementFromPoint(rect.left + Math.min(8, rect.width / 2), rect.top + Math.min(8, rect.height / 2)) !== null;
}

/** Can the user actually reach the message box and its primary action? */
function messageReachable(): boolean {
  const textarea = document.querySelector<HTMLTextAreaElement>("#subagent-message");
  if (textarea === null || !textarea.isConnected) return false;
  const rect = textarea.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) return false;
  if (rect.top < -0.5 || rect.bottom > window.innerHeight + 0.5) return false;
  textarea.focus();
  if (document.activeElement !== textarea) return false;
  const panel = document.querySelector<HTMLElement>('aside[aria-label="会话详情"]');
  const composer = textarea.closest("div.shrink-0");
  if (panel && composer) {
    const panelRect = panel.getBoundingClientRect();
    const composerRect = composer.getBoundingClientRect();
    if (composerRect.bottom > panelRect.bottom + 1 || composerRect.top < panelRect.top - 1) return false;
  }
  const send = [...document.querySelectorAll<HTMLButtonElement>("button")].find(
    (button) => button.textContent?.trim() === "发送",
  );
  if (send === undefined) return false;
  if (!send.disabled && !isVisible(send)) return false;
  return true;
}

Object.assign(window, {
  traceProbe: {
    setTheme(theme: Theme) {
      flushSync(() => applyTheme(theme));
    },
    setScale(scale: FontScale) {
      flushSync(() => applyFontScale(scale));
    },
    setPanelWidth(width: 260 | 360 | 720) {
      // 真实路径是「拖手柄 → 写 localStorage」，面板只在挂载时读，所以重挂一次。
      localStorage.setItem("yi-agent.panelWidth", String(width));
      mutate({ panelKey: state.panelKey + 1 });
    },
    panelWidth() {
      return document.querySelector<HTMLElement>('aside[aria-label="会话详情"]')
        ?.getBoundingClientRect().width ?? 0;
    },
    loadScenario,
    /**
     * One authoritative snapshot with `blocks` folded blocks, as a session that
     * already has a long history would arrive. This is the only shape that can
     * exercise the initial 200-block window: appends deliberately never shrink
     * what the reader already has mounted.
     */
    loadHistory(blocks: number) {
      const rows: AgentTraceRow[] = [];
      let id = 1;
      for (let i = 0; i < blocks; i += 1) {
        // Alternate kinds so the fold produces exactly one block per iteration.
        if (i % 3 === 0) {
          rows.push(row(id++, "tool_result", { name: "read", summary: `第 ${i + 1} 段结果摘要`, is_error: false }));
        } else {
          rows.push(row(id++, "assistant_text", { text: `第 ${i + 1} 段事件正文。` }));
        }
      }
      mutate({
        scenario: "long",
        rows,
        phase: "ready",
        error: null,
        disconnected: false,
        actions: EMPTY_ACTIONS,
        selected: "task-a",
        viewKey: state.viewKey + 1,
      });
    },
    appendText(text: string) {
      const last = state.rows[state.rows.length - 1];
      const nextId = (last?.eventId ?? 0) + 1;
      // Appending adjacent assistant text is exactly what the writer's 512-byte
      // flush produces; the fold must merge it into the trailing block.
      mutate({ rows: [...state.rows, row(nextId, "assistant_text", { text })] });
    },
    appendEvents(count: number) {
      const rows = [...state.rows];
      let id = (rows[rows.length - 1]?.eventId ?? 0) + 1;
      for (let i = 0; i < count; i += 1) {
        rows.push(row(id++, "assistant_text", { text: `追加段落 ${i + 1}：${LONG_CHINESE.slice(0, 40)}` }));
        rows.push(row(id++, "tool_call", { name: "read", summary: `读取文件 src/file-${i + 1}.ts` }));
      }
      mutate({ rows });
    },
    appendToolResult(summary: string) {
      const last = state.rows[state.rows.length - 1];
      const nextId = (last?.eventId ?? 0) + 1;
      mutate({ rows: [...state.rows, row(nextId, "tool_result", { name: "bash", summary, is_error: false })] });
    },
    /**
     * The state right after opening a task: the snapshot has not arrived yet.
     *
     * A new identity matters — the timeline keeps the previous view's window and
     * scroll position across a mere refresh, which is the point of the identity key.
     */
    beginLoading() {
      mutate({
        rows: [],
        phase: "loading",
        error: null,
        disconnected: false,
        selected: "task-a",
        viewKey: state.viewKey + 1,
      });
    },
    setPhase(phase: TraceSubscriptionSnapshot["phase"], message: string | null = null) {
      mutate({ phase, error: phase === "error" ? (message ?? "读取失败") : null });
    },
    setDisconnected(value: boolean) {
      mutate({ disconnected: value });
    },
    setActions(patch: Partial<TraceActionSnapshot>) {
      mutate({ actions: { ...state.actions, ...patch } });
    },
    openTask(taskId: string) {
      mutate({ selected: taskId });
    },
    backToList() {
      mutate({ selected: null });
    },
    scrollTop() {
      return viewport()?.scrollTop ?? 0;
    },
    setScrollTop(top: number) {
      const el = viewport();
      if (el) el.scrollTop = top;
    },
    /** Hit-test geometry for the panel's own viewport, used to aim real wheel input. */
    viewportBox() {
      const el = viewport();
      if (el === null) return null;
      const rect = el.getBoundingClientRect();
      return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
    },
    metrics() {
      const el = viewport();
      const anchor = firstVisibleAnchor();
      return {
        panelOverflow: horizontalOverflow(),
        bodyHeight: el?.clientHeight ?? 0,
        bottomGap: el === null ? Number.POSITIVE_INFINITY : el.scrollHeight - el.clientHeight - el.scrollTop,
        anchorKey: anchor.key,
        anchorOffset: anchor.offset,
        messageReachable: messageReachable(),
        documentOverflow: document.documentElement.scrollWidth - document.documentElement.clientWidth,
        /** Whether the panel is pinned to the newest content (the "回到底部" button is hidden). */
        following: [...document.querySelectorAll("button")].every((b) => b.textContent?.trim() !== "回到底部"),
        scrollTop: el?.scrollTop ?? 0,
      };
    },
    domText() {
      return document.querySelector('aside[aria-label="会话详情"]')?.textContent ?? "";
    },
    timelineCount() {
      return document.querySelectorAll("[data-trace-key]").length;
    },
    hasLoadEarlier() {
      return [...document.querySelectorAll("button")].some((b) => b.textContent?.includes("载入更早"));
    },
    clickLoadEarlier() {
      [...document.querySelectorAll<HTMLButtonElement>("button")]
        .find((b) => b.textContent?.includes("载入更早"))
        ?.click();
    },
    clickText(label: string) {
      [...document.querySelectorAll<HTMLButtonElement>("button")]
        .find((b) => b.textContent?.trim() === label)
        ?.click();
    },
    activeElement() {
      const el = document.activeElement as HTMLElement | null;
      return el === null ? null : { tag: el.tagName.toLowerCase(), label: el.getAttribute("aria-label") ?? el.textContent?.trim() ?? "" };
    },
    /** Put focus where pressing "停止任务" leaves it, without arming a preview. */
    focusStop() {
      const stop = [...document.querySelectorAll<HTMLButtonElement>("button")]
        .find((button) => button.textContent?.trim() === "停止任务");
      stop?.focus();
      return stop !== undefined;
    },
    focusMessage() {
      document.querySelector<HTMLTextAreaElement>("#subagent-message")?.focus();
    },
    /** WCAG relative-luminance contrast of `fg` on the effective background. */
    contrastOf(selector: string) {
      const el = document.querySelector<HTMLElement>(selector);
      if (el === null) return null;
      return contrast(el);
    },
    /** Contrast of every text node the panel actually renders, labelled by content. */
    contrastReport() {
      const panel = document.querySelector<HTMLElement>('aside[aria-label="会话详情"]');
      if (panel === null) return [];
      const out: { label: string; ratio: number; fg: string; bg: number[]; size: string }[] = [];
      for (const el of panel.querySelectorAll<HTMLElement>("*")) {
        const text = (el.textContent ?? "").trim();
        if (text === "") continue;
        // Only leaf-ish nodes that own their text, so one label is not counted twice.
        if (el.children.length > 0) continue;
        const style = getComputedStyle(el);
        if (style.visibility === "hidden" || style.display === "none") continue;
        // Decorative glyphs (`·`, `›`, the colour dot) are explicitly hidden from
        // assistive tech and carry no information — WCAG exempts them.
        if (el.closest('[aria-hidden="true"]') !== null) continue;
        const measured = contrast(el);
        if (measured === null) continue;
        out.push({ label: text.slice(0, 24), ratio: Math.round(measured.ratio * 100) / 100, fg: measured.fg, bg: measured.bg, size: style.fontSize });
      }
      return out;
    },
    mountedCount() {
      return document.querySelectorAll("[data-trace-key]").length;
    },
    /** Open the first collapsed result summary (a real layout change). */
    expandFirstSummary() {
      [...document.querySelectorAll<HTMLButtonElement>("button")]
        .find((b) => b.textContent?.trim() === "展开结果摘要")
        ?.click();
    },
    collapseFirstSummary() {
      [...document.querySelectorAll<HTMLButtonElement>("button")]
        .find((b) => b.textContent?.trim() === "收起结果摘要")
        ?.click();
    },
    expandFirstRaw() {
      [...document.querySelectorAll<HTMLButtonElement>("button")]
        .find((b) => b.textContent?.trim() === "查看原始载荷")
        ?.click();
    },
    /** Simulate the lazy decode of an inline image growing the block after load. */
    growFirstImage(px: number) {
      const img = document.querySelector<HTMLImageElement>("[data-trace-key] img");
      if (img === null) return false;
      img.style.height = `${px}px`;
      return true;
    },
    reducedMotion() {
      return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    },
    accessibleSnapshot() {
      const panel = document.querySelector<HTMLElement>('aside[aria-label="会话详情"]');
      if (panel === null) return null;
      const heading = panel.querySelector("h2");
      const log = panel.querySelector('[role="log"]');
      const textarea = panel.querySelector<HTMLTextAreaElement>("#subagent-message");
      const label = textarea?.labels?.[0]?.textContent?.trim() ?? null;
      const statuses = [...panel.querySelectorAll('[role="status"]')].length;
      const alerts = [...panel.querySelectorAll('[role="alert"]')].length;
      const liveRegions = [...panel.querySelectorAll("[aria-live]")].map((n) => ({
        role: n.getAttribute("role"),
        live: n.getAttribute("aria-live"),
      }));
      return {
        headingTag: heading?.tagName ?? null,
        headingTabIndex: heading?.getAttribute("tabindex") ?? null,
        logName: log?.getAttribute("aria-label") ?? null,
        listItems: panel.querySelectorAll("ol > li").length,
        textareaLabel: label,
        statusRegions: statuses,
        alertRegions: alerts,
        liveRegions,
        /** Streaming text must not be announced: the log is not a live region. */
        logIsLive: log?.hasAttribute("aria-live") ?? false,
      };
    },
    /** Tab from the panel's first focusable control and report where focus lands. */
    tabOrder(limit: number) {
      const panel = document.querySelector<HTMLElement>('aside[aria-label="会话详情"]');
      if (panel === null) return [];
      const focusables = [...panel.querySelectorAll<HTMLElement>(
        'button:not([disabled]), [href], textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
      )].filter((el) => el.getBoundingClientRect().width > 0);
      return focusables.slice(0, limit).map((el) => ({
        tag: el.tagName.toLowerCase(),
        text: (el.textContent ?? "").trim().slice(0, 20),
        ariaLabel: el.getAttribute("aria-label"),
        expanded: el.getAttribute("aria-expanded"),
        hasControls: el.getAttribute("aria-controls") !== null,
      }));
    },
    /** Whether every expandable control advertises its state and target. */
    disclosureContract() {
      const panel = document.querySelector<HTMLElement>('aside[aria-label="会话详情"]');
      if (panel === null) return null;
      const expandables = [...panel.querySelectorAll<HTMLElement>("[aria-expanded]")];
      return expandables.map((el) => ({
        text: (el.textContent ?? "").trim().slice(0, 20),
        expanded: el.getAttribute("aria-expanded"),
        controls: el.getAttribute("aria-controls"),
        targetExists: el.getAttribute("aria-controls") === null
          ? false
          : document.getElementById(el.getAttribute("aria-controls") as string) !== null,
      }));
    },
  },
});

function parseColor(value: string): [number, number, number, number] | null {
  const match = value.match(/rgba?\(([^)]+)\)/);
  if (match === null) return null;
  const parts = match[1].split(",").map((p) => parseFloat(p.trim()));
  if (parts.length < 3) return null;
  return [parts[0], parts[1], parts[2], parts.length > 3 ? parts[3] : 1];
}

function luminance([r, g, b]: [number, number, number, number]): number {
  const channel = (value: number) => {
    const v = value / 255;
    return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
}

/** Composite every translucent ancestor onto one opaque colour. */
function backgroundOf(el: HTMLElement): [number, number, number, number] {
  const stack: [number, number, number, number][] = [];
  let node: HTMLElement | null = el;
  while (node !== null) {
    const parsed = parseColor(getComputedStyle(node).backgroundColor);
    if (parsed !== null && parsed[3] > 0) {
      stack.push(parsed);
      if (parsed[3] >= 1) break;
    }
    node = node.parentElement;
  }
  let base: [number, number, number, number] = [255, 255, 255, 1];
  for (let i = stack.length - 1; i >= 0; i -= 1) {
    const [r, g, b, a] = stack[i];
    base = [
      r * a + base[0] * (1 - a),
      g * a + base[1] * (1 - a),
      b * a + base[2] * (1 - a),
      1,
    ];
  }
  return base;
}

function contrast(el: HTMLElement): { ratio: number; fg: string; bg: number[] } | null {
  const fg = parseColor(getComputedStyle(el).color);
  if (fg === null) return null;
  const bg = backgroundOf(el);
  const composited: [number, number, number, number] = [
    fg[0] * fg[3] + bg[0] * (1 - fg[3]),
    fg[1] * fg[3] + bg[1] * (1 - fg[3]),
    fg[2] * fg[3] + bg[2] * (1 - fg[3]),
    1,
  ];
  const l1 = luminance(composited);
  const l2 = luminance(bg);
  const light = Math.max(l1, l2);
  const dark = Math.min(l1, l2);
  return {
    ratio: (light + 0.05) / (dark + 0.05),
    fg: getComputedStyle(el).color,
    bg: [Math.round(bg[0]), Math.round(bg[1]), Math.round(bg[2])],
  };
}

createRoot(document.getElementById("root")!).render(<Fixture />);
