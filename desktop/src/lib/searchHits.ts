/** 一段命中在源文本里的位置，`end` 为开区间。 */
export type Match = { start: number; end: number };

/**
 * 去掉首尾空白。全空白与空串等价——两者都表示"没在搜"，返回空字符串。
 *
 * 不做小写化：大小写不敏感的比较在 `findMatches` 里用逐次 `toLowerCase` 完成，
 * 因为原文必须按**原始下标**切分，改写查询会丢掉长度对应关系（土耳其语 İ 之类
 * 的大小写折叠还会改变长度）。
 */
export function normalizeQuery(raw: string): string {
  return raw.trim();
}

/**
 * 找出 `query` 在 `text` 里所有出现的位置，大小写不敏感，允许重叠。
 *
 * 允许重叠（`aa` 在 `aaa` 里命中两次）是刻意的：用户搜一个短的重复片段时，
 * 跳过重叠会让计数与"眼睛看到的次数"对不上。
 */
export function findMatches(text: string, query: string): Match[] {
  const needle = normalizeQuery(query);
  if (needle === "") return [];
  const haystack = text.toLowerCase();
  const lower = needle.toLowerCase();
  const out: Match[] = [];
  let from = 0;
  for (;;) {
    const at = haystack.indexOf(lower, from);
    if (at === -1) break;
    out.push({ start: at, end: at + lower.length });
    from = at + 1;
  }
  return out;
}

/**
 * 在 `total` 个命中里移动 `delta` 步，回绕；`total === 0` 时恒为 0。
 *
 * 先对 `current` 取模再移动：调用方可能拿着一个已失效的序号（用户改了查询，
 * 命中数从 20 掉到 3）。夹取放在这里，调用点就不必各写一遍。
 */
export function nextIndex(current: number, total: number, delta: 1 | -1): number {
  if (total <= 0) return 0;
  const base = ((current % total) + total) % total;
  return (base + delta + total) % total;
}

/** mark 相对滚动容器内容顶部的矩形。 */
export type RelRect = { top: number; height: number };
/** 滚动容器的可视区。 */
export type Viewport = { scrollTop: number; clientHeight: number };

/**
 * 打开查找条时"当前命中"落在哪：视口内第一个优先；都不在视口内时，取位于
 * 视口下方最近的一个，否则取最后一个（全在上方）。
 *
 * 为什么不取第一个：长会话里第一个命中常在几万像素之外，打开即跳到顶部会让
 * "我正在读中间这段"的用户丢失位置。浏览器查找栏也是这个行为。
 */
export function initialActiveIndex(rects: RelRect[], viewport: Viewport): number {
  if (rects.length === 0) return 0;
  const top = viewport.scrollTop;
  const bottom = viewport.scrollTop + viewport.clientHeight;
  for (let i = 0; i < rects.length; i += 1) {
    const r = rects[i];
    if (r.top >= top && r.top < bottom) return i;
  }
  for (let i = 0; i < rects.length; i += 1) {
    if (rects[i].top >= bottom) return i;
  }
  return rects.length - 1;
}
