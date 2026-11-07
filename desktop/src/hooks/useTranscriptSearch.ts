import { useCallback, useEffect, useRef, useState } from "react";
import type { RefObject } from "react";
import { initialActiveIndex, nextIndex } from "../lib/searchHits";
import { collectHits, estimatedRects, prefixTops, type SearchHit } from "../lib/searchIndex";
import { normalizeQuery } from "../lib/searchHits";
import type { HeightBook } from "../lib/heightBook";
import type { Item } from "../lib/protocol";
import type { NoticeItem, ReasoningItem } from "../lib/session";

/** 输入防抖。每次敲字都会让可见消息重解析 markdown，120ms 足以吞掉连续击键。 */
export const SEARCH_DEBOUNCE_MS = 120;
/** 命中节点的选择器，测试与实现共用（高亮仍由 rehype 插件产出）。 */
export const SEARCH_HIT_SELECTOR = "mark.search-hit";

type AnyItem = Item | NoticeItem | ReasoningItem;

/**
 * 转录区搜索的运行时状态。
 *
 * **计数与跳转在数据层**（`session.items`），不在 DOM。窗口化之后未挂载的历史
 * 没有任何 `mark` 节点，按 DOM 扫会静默缩小搜索范围——不报错，只是搜不到。
 * 高亮仍由 `rehypeSearchMark` 负责，但那只对已挂载区间生效。
 *
 * 因此「命中数」**不等于**「可见高亮数」：markdown 语法字符、折叠的推理块、
 * 未挂载的历史都会贡献命中却看不到 mark。这是覆盖优先的取舍（见 `searchIndex`）。
 *
 * `containerRef` 必须是**真正滚动**的那个元素（App 传的是 `ChatView` 借出来的
 * `scrollerRef`）。传外层不滚动的容器会让 `scrollTop` 恒为 0，"视口附近"就
 * 退化成"永远第一个"。
 */
export function useTranscriptSearch({
  containerRef,
  open,
  query,
  items,
  structuralVersion,
  book,
  onJump,
}: {
  containerRef: RefObject<HTMLElement | null>;
  open: boolean;
  query: string;
  items: readonly AnyItem[];
  /** 结构版本：条目增删改序时 bump。纯文本 delta 刻意**不**触发重扫。 */
  structuralVersion: number;
  book: HeightBook;
  /** 请求把第 `index` 条 item 摆进视口（通常由宿主转成一次开窗）。 */
  onJump?: (index: number) => void;
}): {
  total: number;
  active: number;
  /**
   * 当前命中的位置（`itemId` + 条内第几个）。`ChatView` 据此给已挂载区间里
   * 对应的 `mark` 打 `data-search-active`——CSS 靠它区分「当前」与「其余」。
   *
   * 未挂载的命中必然为 `null`（没有节点可标），此时靠开窗后自然高亮。
   */
  activeHit: SearchHit | null;
  next: () => void;
  prev: () => void;
  reselect: () => void;
} {
  const [total, setTotal] = useState(0);
  const [active, setActive] = useState(0);
  /**
   * 当前序号的权威副本。
   *
   * 存在的理由：`next`/`prev` 可能在同一帧内被连按两次，此时 `active` 这个
   * state 还是旧的；把跳转放进 `setActive` 的 updater 里也不行——updater
   * 必须是纯函数，StrictMode 会调用两次，副作用会重复执行。
   */
  const activeRef = useRef(0);
  const hitsRef = useRef<SearchHit[]>([]);

  // `onJump` 必须走 ref：调用方几乎总是传内联箭头函数，它的身份每次渲染都变，
  // 若进了 `reveal`/`rescan` 的依赖，就等于**每次渲染都重扫全量历史**。
  const onJumpRef = useRef(onJump);
  onJumpRef.current = onJump;

  /** 把当前命中交给宿主：窗口化后它多半不在挂载区，必须主动开窗。 */
  const reveal = useCallback((index: number) => {
    const hit = hitsRef.current[index];
    if (hit) onJumpRef.current?.(hit.index);
  }, []);

  /**
   * 重扫：在 `session.items` 上匹配。
   *
   * 不扫 DOM 的 `mark.search-hit`：窗口化之后未挂载的历史没有 mark，计数与
   * 跳转都会漏。这里只产出「哪些条命中、第几个字」，高亮交给渲染层。
   */
  const rescan = useCallback(() => {
    const hits = open && normalizeQuery(query) !== "" ? collectHits(items, query) : [];
    hitsRef.current = hits;
    activeRef.current = 0;
    setTotal(hits.length);
    setActive(0);
    // 换查询/结构变更后重新落在第一个命中上。窗口化之后第一个命中很可能不在
    // 挂载区，不主动开窗用户会以为「没找到」。
    if (hits.length > 0) reveal(0);
  }, [items, open, query, reveal]);

  // query / 结构性变更 -> 重扫。**纯文本 delta 刻意不重扫**：流式期间文本按
  // 60ms 窗口更新，重扫 3000 条是纯浪费；搜索是用户主动行为，短暂滞后可接受。
  useEffect(() => {
    rescan();
  }, [rescan, structuralVersion]);

  /** 打开查找条时选「视口附近那个」命中；未挂载的用估算矩形。 */
  const reselect = useCallback(() => {
    const el = containerRef.current;
    const hits = hitsRef.current;
    if (!el || hits.length === 0) {
      activeRef.current = 0;
      setActive(0);
      return;
    }
    const tops = prefixTops(items, book);
    const rects = estimatedRects(hits, tops, book);
    const index = initialActiveIndex(rects, {
      scrollTop: el.scrollTop,
      clientHeight: el.clientHeight,
    });
    activeRef.current = index;
    setActive(index);
    reveal(index);
  }, [book, containerRef, items, reveal]);

  const step = useCallback(
    (delta: 1 | -1) => {
      const totalNow = hitsRef.current.length;
      if (totalNow === 0) {
        activeRef.current = 0;
        setActive(0);
        return;
      }
      const index = nextIndex(activeRef.current, totalNow, delta);
      activeRef.current = index;
      setActive(index);
      reveal(index);
    },
    [reveal],
  );

  const next = useCallback(() => step(1), [step]);
  const prev = useCallback(() => step(-1), [step]);

  return {
    total,
    active,
    activeHit: hitsRef.current[active] ?? null,
    next,
    prev,
    reselect,
  };
}
