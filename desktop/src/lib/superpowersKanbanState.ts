/**
 * 插件 `list` 原始卡（`board.json` 里的形状）→ 看板渲染用的 `BoardCard`。
 *
 * `BoardCard` 定义在这里而不是视图组件里：这是纯数据形状，`lib` 不该反过来
 * 依赖 `components`。视图为兼容旧 import 会 re-export 它。
 */

export interface BoardCard {
  id: string;
  state: string;
  progress: string | null;
  detail: string;
  threadId: string | null;
  /** 标题：spec 文件名去扩展名；无 spec_path 时回退 id。 */
  title?: string;
  specPath?: string;
  planPath?: string;
  workdir?: string;
  kind?: string;
  /** 合并卡源分支。 */
  source?: string;
  /** 合并卡目标分支。 */
  base?: string;
  /** 进入终态的时刻（ISO 串）。插件未提供则不设。 */
  terminalAt?: string;
  /** 入队时刻（ISO 串）。插件未提供则不设。 */
  enqueuedAt?: string;
  /** 队列顺序（越小越前）。插件未提供则不设。 */
  order?: number;
}

export function boardJsonPath(stateDir: string): string {
  return `${stateDir}/board.json`;
}

/** spec 文件名去扩展名；空路径回退 id。`…/2026-10-03-foo.spec.md` → `2026-10-03-foo`。 */
export function deriveTitle(specPath: string, id: string): string {
  if (specPath === "") return id;
  const base = specPath.split("/").pop() ?? specPath;
  return base.replace(/\.(md|markdown)$/i, "").replace(/\.(spec|plan)$/i, "");
}

function optionalString(value: unknown): string | undefined {
  return typeof value === "string" && value !== "" ? value : undefined;
}

/**
 * 归一化一张原始卡。只有缺 `id` 或缺 `state` 时返回 `null`（沿用旧的丢弃规则）；
 * 其余字段缺失都降级，不丢卡——合并卡没有 plan_path 正是这种情况。
 */
export function normalizeCard(raw: unknown): BoardCard | null {
  if (typeof raw !== "object" || raw === null) return null;
  const record = raw as Record<string, unknown>;
  const id = typeof record.id === "string" ? record.id : "";
  const state = typeof record.state === "string" ? record.state : "";
  if (id === "" || state === "") return null;

  const planPath = typeof record.plan_path === "string" ? record.plan_path : "";
  const specPath = typeof record.spec_path === "string" ? record.spec_path : "";
  const workdir = typeof record.workdir === "string" ? record.workdir : "";
  const kind = typeof record.kind === "string" ? record.kind : "implementation";
  const source = typeof record.source === "string" ? record.source : "";
  const base = typeof record.base === "string" ? record.base : "";
  const order = typeof record.order === "number" ? record.order : undefined;
  const threadId =
    typeof record.thread_id === "string" && record.thread_id !== ""
      ? record.thread_id
      : null;

  const detail =
    workdir !== ""
      ? workdir
      : kind === "merge"
        ? `${source} → ${base}`
        : specPath !== ""
          ? specPath
          : planPath;

  return {
    id,
    state: state.toLowerCase(),
    progress: null,
    detail,
    threadId,
    title: deriveTitle(specPath, id),
    specPath: specPath !== "" ? specPath : undefined,
    planPath: planPath !== "" ? planPath : undefined,
    workdir: workdir !== "" ? workdir : undefined,
    kind,
    source: source !== "" ? source : undefined,
    base: base !== "" ? base : undefined,
    terminalAt: optionalString(record.terminal_at),
    enqueuedAt: optionalString(record.enqueued_at),
    order,
  };
}

/**
 * `board.json` 文本 → 卡片数组。复用 `normalizeCard` 保证"原始卡 → 卡片"
 * 只有一处规则；缺 `order` 的卡保持文件序。
 */
export function parseBoard(json: string): BoardCard[] {
  let parsed: unknown;
  try {
    parsed = JSON.parse(json);
  } catch {
    return [];
  }
  if (typeof parsed !== "object" || parsed === null) return [];
  const cards = (parsed as { cards?: unknown }).cards;
  if (!Array.isArray(cards)) return [];

  const mapped: Array<{ card: BoardCard; order: number }> = [];
  for (const raw of cards) {
    const card = normalizeCard(raw);
    if (card === null) continue;
    mapped.push({ card, order: card.order ?? 0 });
  }
  return mapped.sort((left, right) => left.order - right.order).map((entry) => entry.card);
}
