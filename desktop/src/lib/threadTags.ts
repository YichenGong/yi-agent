/**
 * Thread 颜色标签的常量与纯函数。
 *
 * 纯模块：不碰 React、不碰 RPC。
 *
 * **这里是标签默认显示名的唯一权威定义处。** Rust 侧刻意不内置同一套默认名
 * （`settings_store::load_tag_names` 只回用户覆盖项），否则两份中文默认名必须
 * 手工保持同步，迟早漂移。
 */

/** 6 个固定颜色 id，顺序即菜单顺序。**不含灰色**：深色主题下灰点与「无标签」难以区分。 */
export const TAG_IDS = ["red", "orange", "yellow", "green", "blue", "purple"] as const;

export type TagId = (typeof TAG_IDS)[number];

/** 颜色 id → 默认显示名。用户可在设置里覆盖，未覆盖的键回退到这里。 */
export const DEFAULT_TAG_NAMES: Record<TagId, string> = {
  red: "红",
  orange: "橙",
  yellow: "黄",
  green: "绿",
  blue: "蓝",
  purple: "紫",
};

/** 颜色 id → 色点 class。与既有的未读点同尺寸（`size-2 rounded-full`）。 */
export const TAG_DOT_CLASS: Record<TagId, string> = {
  red: "bg-red-400",
  orange: "bg-orange-400",
  yellow: "bg-yellow-400",
  green: "bg-emerald-400",
  blue: "bg-blue-400",
  purple: "bg-violet-400",
};

/** 标签显示名的长度上限，与后端 `TAG_NAME_MAX_LEN` 一致。 */
export const TAG_NAME_MAX_LEN = 24;

/** 类型守卫：服务端字段或手改的 meta 可能是任意值，渲染前必须先过这一关。 */
export function isValidTagId(value: unknown): value is TagId {
  return typeof value === "string" && (TAG_IDS as readonly string[]).includes(value);
}

/**
 * 把服务端给的**部分覆盖**合成为完整的 6 键映射：未提及或空白的键回退默认名。
 * 未知键被丢弃（不透传脏数据到渲染层）。
 */
export function resolveTagNames(
  overrides?: Record<string, string> | null,
): Record<TagId, string> {
  const resolved: Record<TagId, string> = { ...DEFAULT_TAG_NAMES };
  if (!overrides) return resolved;
  for (const id of TAG_IDS) {
    const value = overrides[id];
    if (typeof value === "string" && value.trim() !== "") {
      resolved[id] = value.trim();
    }
  }
  return resolved;
}

/** 取某颜色的显示名；名字表缺键时回退默认名（不抛）。 */
export function tagLabel(id: TagId, names: Record<TagId, string>): string {
  const name = names[id];
  return typeof name === "string" && name.trim() !== "" ? name : DEFAULT_TAG_NAMES[id];
}