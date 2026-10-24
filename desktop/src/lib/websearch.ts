/**
 * 桌面与宿主之间的网页搜索通道。形状与 `mcp.ts` / `skills.ts` 一致：
 * 服务端回手写 `json!`（不走 ts-rs DTO），故本模块做一次形状收敛——
 * 坏行丢掉、缺失字段补安全默认，绝不把半成品往下传。
 */

export type WebSearchCall = (method: string, params: unknown) => Promise<unknown>;

export type Freshness = "noLimit" | "oneDay" | "oneWeek" | "oneMonth" | "oneYear";

const FRESHNESS_VALUES: Freshness[] = [
  "noLimit",
  "oneDay",
  "oneWeek",
  "oneMonth",
  "oneYear",
];

export interface EngineView {
  /** 服务端给的掩码（`••••1234`）。空串表示还没配 key。 */
  apiKeyMasked: string;
  /** 文件里有 key，或环境变量兜底有值。面板据此决定要不要提示「未配置」。 */
  hasKey: boolean;
  baseUrl: string;
  count: number;
  freshness: Freshness;
}

export interface WebSearchView {
  enabled: boolean;
  activeEngine: string;
  /** 键名即引擎 id（目前只有 `bocha`），多引擎时前端按 map 遍历即可。 */
  engines: Record<string, EngineView>;
  path: string;
}

const DEFAULT_ENGINE: EngineView = {
  apiKeyMasked: "",
  hasKey: false,
  baseUrl: "https://api.bocha.cn/v1",
  count: 25,
  freshness: "noLimit",
};

function str(v: unknown, fallback: string): string {
  return typeof v === "string" ? v : fallback;
}

function bool(v: unknown, fallback: boolean): boolean {
  return typeof v === "boolean" ? v : fallback;
}

function clampCount(v: unknown): number {
  if (typeof v !== "number" || !Number.isFinite(v)) return DEFAULT_ENGINE.count;
  return Math.min(50, Math.max(1, Math.trunc(v)));
}

function parseFreshness(v: unknown): Freshness {
  return FRESHNESS_VALUES.includes(v as Freshness) ? (v as Freshness) : "noLimit";
}

function parseEngine(v: unknown): EngineView {
  const o = (v ?? {}) as Record<string, unknown>;
  return {
    apiKeyMasked: str(o.api_key_masked, ""),
    hasKey: bool(o.has_key, false),
    baseUrl: str(o.base_url, DEFAULT_ENGINE.baseUrl),
    count: clampCount(o.count),
    freshness: parseFreshness(o.freshness),
  };
}

export function parseWebSearchView(raw: unknown): WebSearchView {
  const o = (raw ?? {}) as Record<string, unknown>;
  const enginesRaw = o.engines;
  const engines: Record<string, EngineView> = {};
  if (enginesRaw && typeof enginesRaw === "object" && !Array.isArray(enginesRaw)) {
    for (const [id, value] of Object.entries(enginesRaw as Record<string, unknown>)) {
      engines[id] = parseEngine(value);
    }
  }
  // bocha 永远有一份视图：面板绑定它，缺了就会崩。
  if (!engines.bocha) engines.bocha = { ...DEFAULT_ENGINE };
  return {
    enabled: bool(o.enabled, true),
    activeEngine: str(o.active_engine, "bocha"),
    engines,
    path: str(o.path, ""),
  };
}

export interface TestResult {
  ok: boolean;
  message: string;
  elapsedMs: number;
}

export function parseTestResult(raw: unknown): TestResult {
  const o = (raw ?? {}) as Record<string, unknown>;
  return {
    ok: bool(o.ok, false),
    message: str(o.message, "测试失败：响应无法解析"),
    elapsedMs: typeof o.elapsed_ms === "number" ? o.elapsed_ms : 0,
  };
}
