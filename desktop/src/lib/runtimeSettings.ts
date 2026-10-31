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
}

export interface RuntimeSettingsView {
  fields: RuntimeField[];
  path: string;
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
  return {
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
}

export function parseRuntimeSettings(view: unknown): RuntimeSettingsView {
  if (typeof view !== "object" || view === null) return { fields: [], path: "" };
  const record = view as Record<string, unknown>;
  const fields = Array.isArray(record.fields)
    ? record.fields
        .map(parseField)
        .filter((field): field is RuntimeField => field !== null)
    : [];
  return { fields, path: str(record.path) };
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
