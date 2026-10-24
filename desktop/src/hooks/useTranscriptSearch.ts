import { useCallback, useEffect, useRef, useState } from "react";
import type { RefObject } from "react";
import { initialActiveIndex, nextIndex } from "../lib/searchHits";
import type { RelRect } from "../lib/searchHits";

/** 输入防抖。每次敲字都会让可见消息重解析 markdown，120ms 足以吞掉连续击键。 */
export const SEARCH_DEBOUNCE_MS = 120;
/** 命中节点的选择器，测试与实现共用。 */
export const SEARCH_HIT_SELECTOR = "mark.search-hit";

/**
 * 转录区搜索的运行时状态。
 *
 * 分工：**渲染**由 `query` 驱动（rehype 插件生成 mark），**计数与当前命中**由
 * 这里渲染后扫 DOM 得出。两者单向：扫描只读 DOM，从不回写渲染输入，因此不会
 * 形成"扫描 → 改状态 → 重渲染 → 再扫描"的环。
 *
 * 当前命中靠 `data-search-active` 属性标注，而不是在 state 里存元素引用：
 * mark 节点会随任一重渲染被整体替换，存下来的引用必然失效。
 *
 * `containerRef` 必须是**真正滚动**的那个元素（App 传的是 `ChatView` 借出来的
 * `scrollerRef`）。传外层不滚动的容器会让 `scrollTop` 恒为 0，"视口附近"就
 * 退化成"永远第一个"。
 */
export function useTranscriptSearch({
  containerRef,
  open,
  query,
}: {
  containerRef: RefObject<HTMLElement | null>;
  open: boolean;
  query: string;
}): {
  total: number;
  active: number;
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
   * state 还是旧的；把 DOM 标注放进 `setActive` 的 updater 里也不行——updater
   * 必须是纯函数，StrictMode 会调用两次，副作用会重复执行。
   */
  const activeRef = useRef(0);

  const hits = useCallback((): HTMLElement[] => {
    const el = containerRef.current;
    if (!el) return [];
    return Array.from(el.querySelectorAll<HTMLElement>(SEARCH_HIT_SELECTOR));
  }, [containerRef]);

  /** 把第 `index` 个命中设为当前，并按需滚到视野中央。 */
  const apply = useCallback((all: HTMLElement[], index: number, scroll: boolean) => {
    for (let i = 0; i < all.length; i += 1) {
      if (i === index) all[i].setAttribute("data-search-active", "true");
      else all[i].removeAttribute("data-search-active");
    }
    activeRef.current = index;
    if (scroll && all[index]) all[index].scrollIntoView({ block: "center" });
  }, []);

  /**
   * 清掉所有标注。关闭搜索时用。
   *
   * **不能靠给 `apply` 传空数组来清**：那样循环体一次都不跑，旧的
   * `data-search-active` 会原样留在 DOM 上。必须先把节点查出来再逐个摘。
   */
  const clear = useCallback(() => {
    const all = hits();
    for (const node of all) node.removeAttribute("data-search-active");
    activeRef.current = 0;
    setTotal(0);
    setActive(0);
  }, [hits]);

  /** 按视口重选当前命中。查询变化、渲染落地后由宿主调用。 */
  const reselect = useCallback(() => {
    const el = containerRef.current;
    const all = hits();
    setTotal(all.length);
    if (!el || all.length === 0) {
      apply(all, 0, false);
      setActive(0);
      return;
    }
    const box = el.getBoundingClientRect();
    const rects: RelRect[] = all.map((node) => {
      const r = node.getBoundingClientRect();
      return { top: r.top - box.top + el.scrollTop, height: r.height };
    });
    const index = initialActiveIndex(rects, {
      scrollTop: el.scrollTop,
      clientHeight: el.clientHeight,
    });
    apply(all, index, true);
    setActive(index);
  }, [apply, containerRef, hits]);

  useEffect(() => {
    if (!open) {
      clear();
      return;
    }
    // 渲染已落地（本 effect 在提交后运行）：计一次数并标第一个，随后由宿主的
    // `reselect` 按视口重选——这里不做滚动，避免刚打开就跳。
    const all = hits();
    setTotal(all.length);
    apply(all, 0, false);
    setActive(0);
  }, [open, query, hits, apply, clear]);

  const step = useCallback(
    (delta: 1 | -1) => {
      const all = hits();
      setTotal(all.length);
      if (all.length === 0) {
        apply([], 0, false);
        setActive(0);
        return;
      }
      const index = nextIndex(activeRef.current, all.length, delta);
      apply(all, index, true);
      setActive(index);
    },
    [apply, hits],
  );

  const next = useCallback(() => step(1), [step]);
  const prev = useCallback(() => step(-1), [step]);

  return { total, active, next, prev, reselect };
}
