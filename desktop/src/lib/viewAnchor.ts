/**
 * 会话页的阅读位置：视口顶部那条消息 + 它相对视口顶部的偏移。
 *
 * 为什么锚消息而不是记像素：同一条会话在后台会继续追加内容，像素位置会在
 * 每次进入后指向另一段文字；消息 id 在服务端重放时保持稳定，锚它才能"回到
 * 我原来在读的那条"。
 *
 * 贴底不在这里表达——那是 `null`（见 `ChatView` 的 `anchor` 属性）。
 */
export interface ViewAnchor {
  /** 协议里的 item id。只会是服务端 id，不会是 `local-*`。 */
  itemId: string;
  /** 该消息顶边相对视口顶部的偏移（负数 = 已滚过它的顶边）。 */
  offset: number;
}

/** 一条消息当前的纵向位置：`top` 是相对滚动内容顶部的距离。 */
export interface ItemBox {
  id: string;
  top: number;
}

/**
 * 本地乐观项（`local-*`）不入锚：它由 `Session` 在本地分配序号，冷启动与历史
 * 重放后会重建，用它做锚在第二次进入时必然找不到目标。
 */
export function isAnchorableId(id: string | undefined): id is string {
  return typeof id === "string" && id.length > 0 && !id.startsWith("local-");
}

/**
 * 视口顶部那条消息：顶边在 `scrollTop` 之上（含相等）的**最后**一条。
 *
 * 允许 1px 误差：带边框/亚像素的行高会让"正好在顶部"落成 199.5，严格比较会在
 * 这种情况下退到上一条，读起来就差一行。
 */
export function findAnchor(boxes: ItemBox[], scrollTop: number): ViewAnchor | null {
  let best: ItemBox | null = null;
  // 没有任何一条「顶边已在视口顶之上」时的退路：视口里最靠上的那条。
  // 典型情形是滚到最顶——滚动容器带 `py-3`（12px）内边距，首条消息的内容坐标
  // 是 +12，若此时返回 `null`，宿主会把它读成「贴底」，切回来就跳到底部，
  // 与用户「正在读最老的那条」的意图正好相反。
  let first: ItemBox | null = null;
  for (const box of boxes) {
    if (!isAnchorableId(box.id)) continue;
    if (first === null || box.top < first.top) first = box;
    if (box.top > scrollTop + 1) continue;
    if (best === null || box.top >= best.top) best = box;
  }
  const chosen = best ?? first;
  if (chosen === null) return null;
  return { itemId: chosen.id, offset: chosen.top - scrollTop };
}

/**
 * 恢复时的目标 `scrollTop`。`box` 缺失（消息被删/历史被截断）或不是可锚 id 时
 * 返回 `null`，由调用方回退到底部——宁可看到最新的，也不要停在无意义的像素上。
 */
export function restoreScrollTop(box: ItemBox | undefined, anchor: ViewAnchor): number | null {
  if (!box || !isAnchorableId(box.id)) return null;
  return Math.max(0, box.top - anchor.offset);
}
