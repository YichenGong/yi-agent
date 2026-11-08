/** 转录几何测量的类型与唯一 DOM 读取入口。零副作用，便于纯函数测试。 */

export type TranscriptRegion = "history" | "tail";

/** 一行已挂载消息的几何（内容坐标系，px）。`index` 是绝对 item index。 */
export interface RowGeometry {
  id: string;
  index: number;
  top: number;
  bottom: number;
  region: TranscriptRegion;
}

/** 历史区首末行相对视口的有符号 gap。不含 LiveTail。 */
export interface HistoryEdge {
  /** 历史区第一行的绝对 index。 */
  first: number;
  /** 历史区最后一行的绝对 index。 */
  last: number;
  /** `historyFirst.top - viewportTop`；正值=视口已越过历史顶边。 */
  gapTopPx: number;
  /** `viewportBottom - historyLast.bottom`；正值=视口已越过历史底边。 */
  gapBottomPx: number;
}

export interface WindowMeasurement {
  /** 与视口相交的行，按绝对 index 取首末；无相交行为 null。 */
  visible: { first: number; last: number } | null;
  /** 历史区边界；历史区一行都没挂载时为 null（只有 LiveTail）。 */
  history: HistoryEdge | null;
  tailVisible: boolean;
  viewportTop: number;
  viewportBottom: number;
}

/** 从挂载行读出的最小几何；`top`/`bottom` 是**内容坐标**。 */
function readRows(scroller: HTMLElement): Array<{
  index: number;
  top: number;
  bottom: number;
  region: TranscriptRegion;
}> {
  const base = scroller.getBoundingClientRect().top - scroller.scrollTop;
  const rows: Array<{ index: number; top: number; bottom: number; region: TranscriptRegion }> = [];
  for (const node of scroller.querySelectorAll<HTMLElement>("[data-item-id]")) {
    const raw = node.dataset.itemIndex;
    if (raw === undefined) continue;
    const index = Number(raw);
    if (!Number.isFinite(index)) continue;
    const r = node.getBoundingClientRect();
    if (r.height <= 0) continue;
    rows.push({
      index,
      top: r.top - base,
      bottom: r.bottom - base,
      region: node.dataset.transcriptRegion === "tail" ? "tail" : "history",
    });
  }
  return rows;
}

/**
 * 测量当前转录视口。
 *
 * 三条口径都是真实缺陷的边界：
 * 1. `visible` 用 `data-item-index` 的**绝对**序号，不再让 hook 用
 *    `window.start + 相对序号` 二次换算（tail 不在历史内，换算是错的）。
 * 2. `history` 只统计历史区行。以前取「首末挂载行」，恒挂在 400 条之外的
 *    LiveTail 会把底边拉到视口内，读成「已覆盖」，于是不再向下开窗。
 * 3. gap 有符号且以真实 viewport 为基准：负值 = 视口已越过该边界。
 *
 * 未布局（首帧、jsdom、clientHeight 为 0、没有任何可读行）返回 null：
 * 调用方应暂停开窗，但**不能**把它读成「用户要贴底」。
 */
export function measureTranscript(scroller: HTMLElement | null): WindowMeasurement | null {
  if (!scroller || scroller.clientHeight <= 0) return null;
  const rows = readRows(scroller);
  if (rows.length === 0) return null;

  const viewportTop = scroller.scrollTop;
  const viewportBottom = viewportTop + scroller.clientHeight;

  let visibleFirst: number | null = null;
  let visibleLast: number | null = null;
  let historyFirst: (typeof rows)[number] | null = null;
  let historyLast: (typeof rows)[number] | null = null;
  let tailVisible = false;

  for (const row of rows) {
    if (row.bottom > viewportTop && row.top < viewportBottom) {
      if (visibleFirst === null || row.index < visibleFirst) visibleFirst = row.index;
      if (visibleLast === null || row.index > visibleLast) visibleLast = row.index;
      if (row.region === "tail") tailVisible = true;
    }
    if (row.region !== "history") continue;
    if (historyFirst === null || row.index < historyFirst.index) historyFirst = row;
    if (historyLast === null || row.index > historyLast.index) historyLast = row;
  }

  return {
    visible:
      visibleFirst === null || visibleLast === null
        ? null
        : { first: visibleFirst, last: visibleLast },
    history:
      historyFirst === null || historyLast === null
        ? null
        : {
            first: historyFirst.index,
            last: historyLast.index,
            gapTopPx: historyFirst.top - viewportTop,
            gapBottomPx: viewportBottom - historyLast.bottom,
          },
    tailVisible,
    viewportTop,
    viewportBottom,
  };
}
