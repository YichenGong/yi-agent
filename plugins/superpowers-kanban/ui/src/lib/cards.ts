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
  /** 提出这张卡的讨论会话（pre-queued 卡）。插件未提供则为 null。 */
  originThreadId: string | null;
  /** 标题：显式 `title` 优先；否则 spec 文件名去扩展名，无 spec_path 时回退 id。 */
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
  /**
   * 卡写着 running，但宿主报这个会话已经不在了。**只是角标**：插件不会因此
   * 改状态，卡片仍是 running，等人或模型决定下一步。
   *
   * 旧插件只给这个布尔；新插件会同时给 `sessionObservation`。两者都在时以新字段
   * 为准（`stale` 保留给旧调用方与旧插件）。
   */
  stale?: boolean;
  /** 该会话最后一次被看到还活着的时刻（ISO 串）。只用于 stale 卡的说明。 */
  lastAliveAt?: string;
  /**
   * 当前宿主热表里有没有这张卡的执行会话：
   * - `loaded`：在（无论闲不闲）；
   * - `not_loaded`：**明确**不在——历史可能仍在磁盘上；
   * - `unknown`：没有有效证据（旧格式、查询失败、这一拍没问）。
   *
   * **缺键 ≠ `unknown`**：旧插件根本不发这个键，那种情况下我们只能回退到
   * `stale` 布尔。所以这里保持 `undefined`，不预归一化。
   */
  sessionObservation?: "loaded" | "not_loaded" | "unknown";
  /** 这条加载观测的采集时刻（RFC3339）。`unknown` 时插件不给。 */
  observedAt?: string;
  /** 排进合并队列的时刻（ISO 串）。插件未提供则不设。 */
  mergeQueuedAt?: string;
  /**
   * 本项目此刻的合并名额在它手里。合并是一次会话回合，所以这时卡仍是
   * `running`——光看状态分不出「在写代码」和「在合并」。
   */
  merging?: boolean;
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
 * 加载态字段的归一化。
 *
 * 三重区分是**故意**的：
 * - 键缺失（`undefined`）→ 旧插件，留给调用方回退到 `stale` 布尔；
 * - 键存在但值不合法（null、数字、未知字符串）→ `unknown`，因为新插件确实发了
 *   这个字段，只是我们读不懂——这时绝不能假装"没问过旧插件"；
 * - 合法值原样保留。
 */
function optionalObservation(
  value: unknown,
): "loaded" | "not_loaded" | "unknown" | undefined {
  if (value === undefined) return undefined;
  if (value === "loaded" || value === "not_loaded" || value === "unknown") return value;
  return "unknown";
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
  const originThreadId =
    typeof record.origin_thread_id === "string" && record.origin_thread_id !== ""
      ? record.origin_thread_id
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
    originThreadId,
    title:
      typeof record.title === "string" && record.title !== ""
        ? record.title
        : deriveTitle(specPath, id),
    specPath: specPath !== "" ? specPath : undefined,
    planPath: planPath !== "" ? planPath : undefined,
    workdir: workdir !== "" ? workdir : undefined,
    kind,
    source: source !== "" ? source : undefined,
    base: base !== "" ? base : undefined,
    terminalAt: optionalString(record.terminal_at),
    enqueuedAt: optionalString(record.enqueued_at),
    order,
    stale: record.stale === true,
    lastAliveAt: optionalString(record.last_alive_at),
    sessionObservation: optionalObservation(record.session_observation),
    observedAt: optionalString(record.observed_at),
    mergeQueuedAt: optionalString(record.merge_queued_at),
    merging: record.merging === true,
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
