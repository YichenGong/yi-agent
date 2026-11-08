/**
 * 桌面端与宿主之间的运行参数通道（`settings/read` / `settings/write`）。
 *
 * 形状与 `websearch.ts` / `mcp.ts` 一致：服务端回手写 `json!`（不走 ts-rs DTO），
 * 故本模块做一次形状收敛——坏行丢掉、缺失字段补安全默认，绝不把半成品往下传。
 *
 * **`value` 的 `null` 必须原样保留。** 它是「未设置」，与 `0`、`""` 都不同：
 * 归一成默认值就会重演旧 `get_config` 的问题——保存一次后所有字段都显示成已设置。
 */

export type RuntimeSettingsCall = (method: string, params: unknown) => Promise<unknown>;

export type RuntimeFieldType = "number" | "text" | "select";

/**
 * 保存之后这项在什么时候落地。
 *
 * `next_safe_point`：已运行的对话在下一个安全执行点采用（用户通常感知不到）。
 * `next_session`：只影响之后新建的会话（例如默认沙箱，已有会话用固化基线）。
 */
export type RuntimeApplyPolicy = "next_safe_point" | "next_session";

/** 宿主这一项的有效值来自哪一层。 */
export type RuntimeHostSource = "override" | "environment" | "settings" | "default";

export interface RuntimeField {
  key: string;
  group: string;
  label: string;
  type: RuntimeFieldType;
  /** `null` = 未设置。数字 0 与空串是不同的值，不要用真值判断。 */
  value: string | number | null;
  /** 默认值的展示文案（服务端常量）。 */
  default: string;
  description: string;
  options: string[];
  min: number | null;
  max: number | null;
  /** 旧宿主没有这些字段；缺失即 `undefined`，不要补成默认策略。 */
  apply_policy?: RuntimeApplyPolicy;
  /** 宿主该字段的有效默认值。**不是**某个已有会话的生效值。 */
  host_value?: string | number | null;
  host_source?: RuntimeHostSource;
}

export type RuntimeSnapshotOrigin = "current" | "last_known_good" | "startup_fallback";

export interface RuntimeHealth {
  state: "healthy" | "degraded";
  /** 安全错误类别。绝不包含设置值。 */
  code: string | null;
  invalid_keys: string[];
}

export interface RuntimeSettingsView {
  fields: RuntimeField[];
  path: string;
  /** 只有宿主明确报 `1` 才有值。缺失/未知版本的宿主都读作不支持。 */
  live_apply_version?: 1;
  /** 便捷判定：`live_apply_version === 1`。 */
  supported: boolean;
  revision?: string;
  snapshot_origin?: RuntimeSnapshotOrigin;
  health?: RuntimeHealth;
}

/** `settings/write` 的回执。`ok` 只表示**已提交**，不是全体已采用。 */
export interface RuntimeSettingsWriteResult {
  ok: boolean;
  live_apply_version?: 1;
  revision?: string;
  changed_keys: string[];
  apply_policy: Record<string, RuntimeApplyPolicy>;
  overridden_keys: Array<{ key: string; source: "override" | "environment" }>;
}

const FIELD_TYPES: RuntimeFieldType[] = ["number", "text", "select"];

function str(v: unknown, fallback = ""): string {
  return typeof v === "string" ? v : fallback;
}

function numOrNull(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** `null` / 数字 / 字符串三种都合法；其它（对象、布尔）按「未设置」处理。 */
function scalarOrNull(v: unknown): string | number | null {
  if (v === null || v === undefined) return null;
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string") return v;
  return null;
}

const APPLY_POLICIES: RuntimeApplyPolicy[] = ["next_safe_point", "next_session"];
const HOST_SOURCES: RuntimeHostSource[] = ["override", "environment", "settings", "default"];
const SNAPSHOT_ORIGINS: RuntimeSnapshotOrigin[] = [
  "current",
  "last_known_good",
  "startup_fallback",
];

function oneOf<T extends string>(value: unknown, allowed: T[]): T | undefined {
  return typeof value === "string" && allowed.includes(value as T) ? (value as T) : undefined;
}

/**
 * 只认 `1`。
 *
 * "缺失 = 支持" 是最危险的默认：旧宿主恰好缺这个字段，UI 就会承诺一个它做不到的
 * 热更新。未知的更大版本号同理——本客户端不认识那个方言，不能冒充认识。
 */
function parseLiveApplyVersion(value: unknown): 1 | undefined {
  return value === 1 ? 1 : undefined;
}

function parseHealth(value: unknown): RuntimeHealth | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const record = value as Record<string, unknown>;
  const state = oneOf(record.state, ["healthy", "degraded"] as const);
  if (!state) return undefined;
  return {
    state,
    code: typeof record.code === "string" ? record.code : null,
    invalid_keys: Array.isArray(record.invalid_keys)
      ? record.invalid_keys.filter((key): key is string => typeof key === "string")
      : [],
  };
}

function parseField(raw: unknown): RuntimeField | null {
  if (typeof raw !== "object" || raw === null) return null;
  const record = raw as Record<string, unknown>;
  const key = str(record.key);
  if (!key) return null;
  const type = FIELD_TYPES.includes(record.type as RuntimeFieldType)
    ? (record.type as RuntimeFieldType)
    : "text";
  const options = Array.isArray(record.options)
    ? record.options.filter((o): o is string => typeof o === "string")
    : [];
  const field: RuntimeField = {
    key,
    group: str(record.group),
    label: str(record.label, key),
    type,
    value: scalarOrNull(record.value),
    default: str(record.default),
    description: str(record.description),
    options,
    min: numOrNull(record.min),
    max: numOrNull(record.max),
  };
  // Only assign recognised values: an unknown policy must stay absent rather than
  // become a string the UI would then render as if it knew what it meant.
  const applyPolicy = oneOf(record.apply_policy, APPLY_POLICIES);
  if (applyPolicy) field.apply_policy = applyPolicy;
  const hostSource = oneOf(record.host_source, HOST_SOURCES);
  if (hostSource) field.host_source = hostSource;
  if ("host_value" in record) field.host_value = scalarOrNull(record.host_value);
  return field;
}

export function parseRuntimeSettings(view: unknown): RuntimeSettingsView {
  if (typeof view !== "object" || view === null) {
    return { fields: [], path: "", supported: false };
  }
  const record = view as Record<string, unknown>;
  const fields = Array.isArray(record.fields)
    ? record.fields
        .map(parseField)
        .filter((field): field is RuntimeField => field !== null)
    : [];
  const liveApplyVersion = parseLiveApplyVersion(record.live_apply_version);
  const result: RuntimeSettingsView = {
    fields,
    path: str(record.path),
    supported: liveApplyVersion === 1,
  };
  if (liveApplyVersion) result.live_apply_version = liveApplyVersion;
  if (typeof record.revision === "string") result.revision = record.revision;
  const origin = oneOf(record.snapshot_origin, SNAPSHOT_ORIGINS);
  if (origin) result.snapshot_origin = origin;
  const health = parseHealth(record.health);
  if (health) result.health = health;
  return result;
}

/**
 * 解析 `settings/write` 的回执。
 *
 * 旧宿主只回 `{ok: true}`，此时必须读成「已保存，但没有热更新保证」——绝不能因为
 * 缺字段就当成支持。
 */
export function parseRuntimeSettingsWrite(value: unknown): RuntimeSettingsWriteResult {
  if (typeof value !== "object" || value === null) {
    return {
      ok: false,
      live_apply_version: undefined,
      revision: undefined,
      changed_keys: [],
      apply_policy: {},
      overridden_keys: [],
    };
  }
  const record = value as Record<string, unknown>;
  const liveApplyVersion = parseLiveApplyVersion(record.live_apply_version);
  const applyPolicy: Record<string, RuntimeApplyPolicy> = {};
  if (typeof record.apply_policy === "object" && record.apply_policy !== null) {
    for (const [key, policy] of Object.entries(record.apply_policy as Record<string, unknown>)) {
      const parsed = oneOf(policy, APPLY_POLICIES);
      if (parsed) applyPolicy[key] = parsed;
    }
  }
  const overridden: Array<{ key: string; source: "override" | "environment" }> = [];
  if (Array.isArray(record.overridden_keys)) {
    for (const entry of record.overridden_keys) {
      if (typeof entry !== "object" || entry === null) continue;
      const item = entry as Record<string, unknown>;
      if (typeof item.key !== "string") continue;
      const source = oneOf(item.source, ["override", "environment"] as const);
      if (source) overridden.push({ key: item.key, source });
    }
  }
  const result: RuntimeSettingsWriteResult = {
    ok: record.ok === true,
    live_apply_version: liveApplyVersion,
    revision: typeof record.revision === "string" ? record.revision : undefined,
    changed_keys: Array.isArray(record.changed_keys)
      ? record.changed_keys.filter((key): key is string => typeof key === "string")
      : [],
    apply_policy: applyPolicy,
    overridden_keys: overridden,
  };
  return result;
}

/**
 * 算出「用户改了哪几项」。`drafts` 里没有的键视为未改动——这样用户只动一个
 * 输入框时，不会把其余字段的当前值整体回写一遍。
 *
 * 输入框是空串时发 `null`（删键、回退默认），而不是发空串让服务端去猜。
 */
export function buildPatch(
  original: RuntimeField[],
  drafts: Record<string, string>,
): Record<string, string | null> {
  const patch: Record<string, string | null> = {};
  for (const field of original) {
    if (!(field.key in drafts)) continue;
    const draft = drafts[field.key];
    const current = field.value === null ? "" : String(field.value);
    if (draft === current) continue;
    patch[field.key] = draft === "" ? null : draft;
  }
  return patch;
}
