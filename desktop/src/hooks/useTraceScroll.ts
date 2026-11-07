/**
 * 本栏自己的阅读锚点。
 *
 * 共享的 `useStickToBottom` 只回答一件事：用户还想不想看最新内容。它做不到的
 * 是「内容变高之后，把用户刚在看的**那一段**留在原地」——展开、载入更早的
 * 200 段、图片撑开高度，都会把视口内容整体推走。这里补上那一层：
 *
 * * **锚点**是 `{eventKey, offsetPx}`：视口顶部第一个相交的 `[data-trace-key]`
 *   及其相对视口的偏移。恢复时把 `scrollTop` 调整 `新偏移 - 旧偏移`，于是那一段
 *   仍然停在原来的高度上。
 * * **程序滚动守卫**：我们写 `scrollTop` 之后浏览器才异步派发 `scroll`，那时看着
 *   像用户滑回底部。没有守卫，一次「载入更早」就会把暂停状态悄悄解除，接着每个
 *   delta 都把人拽到底部。只有真实的 wheel/touch/按键/拖动才算用户意图。
 * * **锚点必须在布局变化之前取**。异步回调（ResizeObserver、图片 load）里再取，
 *   量到的已经是变化后的位置，补偿等于没做；因此这里只缓存「上一次已知的锚点」，
 *   由调用方在 setState 前通过 `preserve` 主动更新。
 *
 * 不修改 `useStickToBottom`：它是主对话与若干面板共用的，语义已被测试钉住。
 */
import { useCallback, useEffect, useRef, useState } from "react";
import type { RefObject } from "react";

import { useStickToBottom } from "../lib/useStickToBottom";

export interface TraceScroll {
  ref: RefObject<HTMLDivElement | null>;
  /** 全部滚动内容（含子任务区与尾部状态），供 ResizeObserver 观察。 */
  contentRef: RefObject<HTMLDivElement | null>;
  taskInfoRef: RefObject<HTMLDivElement | null>;
  taskInfoHeadingRef: RefObject<HTMLHeadingElement | null>;
  /** 是否仍在跟随最新内容。**state**，因为浮钮的显示要触发渲染。 */
  following: boolean;
  followIfAtBottom(): void;
  scrollToBottom(): void;
  /** 在 DOM 变化前记锚点、变化后恢复；调用方把变更包在 change 里。 */
  preserve(change: () => void): void;
  showTaskInfo(reveal: () => void, entry: HTMLButtonElement): void;
  restoreTaskInfo(hide: () => void): void;
  /** 供测试与详情层判断当前视图属于哪一次选择。 */
  identityKey: string;
}

interface Anchor {
  eventKey: string;
  offsetPx: number;
}

/** 真实用户意图。只有这些事件能解除程序滚动的暂停守卫。 */
const INTENT_EVENTS = ["wheel", "touchstart", "touchmove", "keydown", "pointerdown"] as const;

export function useTraceScroll(
  identityKey: string,
  ready: boolean,
  onFallbackFocus: () => void,
): TraceScroll {
  const stick = useStickToBottom();
  const contentRef = useRef<HTMLDivElement | null>(null);
  const taskInfoRef = useRef<HTMLDivElement | null>(null);
  const taskInfoHeadingRef = useRef<HTMLHeadingElement | null>(null);

  const [following, setFollowing] = useState(false);
  /** 上一次已知的锚点：在布局变化**之前**记下，异步回调里用它。 */
  const anchorRef = useRef<Anchor | null>(null);
  /** 本轮程序滚动尚未消化；期间的 scroll 事件不改变跟随意图。 */
  const programmaticRef = useRef(false);
  /** 真实滚动意图的序号；程序滚动只在它不变时才保持暂停。 */
  const intentRef = useRef(0);
  const pendingFollowRef = useRef(false);
  const entryRef = useRef<HTMLButtonElement | null>(null);
  const infoAnchorRef = useRef<Anchor | null>(null);

  const readAnchor = useCallback((): Anchor | null => {
    const el = stick.ref.current;
    if (!el) return null;
    const viewportTop = el.getBoundingClientRect().top;
    const nodes = el.querySelectorAll<HTMLElement>("[data-trace-key]");
    for (const node of nodes) {
      const rect = node.getBoundingClientRect();
      if (rect.bottom <= viewportTop) continue;
      const key = node.dataset.traceKey;
      if (key === undefined) continue;
      return { eventKey: key, offsetPx: rect.top - viewportTop };
    }
    return null;
  }, [stick.ref]);

  /** 把缓存锚点恢复成同样的偏移；找不到那一段时退到最近的保留段。 */
  const restoreAnchor = useCallback((anchor: Anchor | null) => {
    const el = stick.ref.current;
    if (!el || anchor === null) return;
    const nodes = [...el.querySelectorAll<HTMLElement>("[data-trace-key]")];
    if (nodes.length === 0) return;

    const exact = nodes.find((node) => node.dataset.traceKey === anchor.eventKey);
    const target = exact ?? nodes[0];
    const viewportTop = el.getBoundingClientRect().top;
    const delta = target.getBoundingClientRect().top - viewportTop - anchor.offsetPx;
    if (delta === 0) return;
    el.scrollTop += delta;
  }, [stick.ref]);

  const markProgrammatic = useCallback((run: () => void) => {
    const el = stick.ref.current;
    programmaticRef.current = true;
    const intentAtStart = intentRef.current;
    run();
    // 让浏览器先把这次赋值对应的 scroll 消化掉，再允许真实意图接管。
    if (el) el.scrollTop = el.scrollTop;
    queueMicrotask(() => {
      if (intentRef.current === intentAtStart) programmaticRef.current = false;
    });
  }, [stick.ref]);

  // 跟随意图：共享 hook 维护它，这里复制一份成 state，浮钮才能随之重新渲染。
  useEffect(() => {
    const el = stick.ref.current;
    if (!el) return;

    const sync = () => {
      const next = stick.isFollowing();
      setFollowing((previous) => (previous === next ? previous : next));
    };

    const onIntent = () => {
      intentRef.current += 1;
      programmaticRef.current = false;
    };

    const onScroll = () => {
      // 程序滚动期间不写意图：那是我们自己造成的位置变化。
      if (programmaticRef.current) return;
      if (pendingFollowRef.current) {
        pendingFollowRef.current = false;
        sync();
        return;
      }
      // 真实的上翻之后才更新缓存锚点。这里不能覆盖 pre-layout 锚点：
      // 布局变化期间的 scroll 事件不该把锚点读成「变化后」的位置。
      anchorRef.current = readAnchor();
      sync();
    };

    el.addEventListener("scroll", onScroll, { passive: true });
    for (const type of INTENT_EVENTS) {
      el.addEventListener(type, onIntent, { passive: true });
    }
    sync();

    return () => {
      el.removeEventListener("scroll", onScroll);
      for (const type of INTENT_EVENTS) {
        el.removeEventListener(type, onIntent);
      }
    };
  }, [readAnchor, stick, stick.ref]);

  // 内容/视口尺寸变化：跟随时贴底，暂停时按**变化前**的缓存锚点恢复。
  useEffect(() => {
    const content = contentRef.current;
    if (!content || typeof ResizeObserver === "undefined") return;

    const observer = new ResizeObserver(() => {
      if (pendingFollowRef.current) return;
      if (stick.isFollowing()) {
        pendingFollowRef.current = true;
        stick.followIfAtBottom();
        setFollowing(true);
        return;
      }
      // 锚点取的是变化前缓存的那一份：此刻再量已经晚了。
      const anchor = anchorRef.current;
      markProgrammatic(() => restoreAnchor(anchor));
    });
    observer.observe(content);
    const el = stick.ref.current;
    if (el) observer.observe(el);

    return () => observer.disconnect();
  }, [markProgrammatic, restoreAnchor, stick]);

  // 切换任务/重新加载：锚点属于上一条时间线，不能带过来。
  useEffect(() => {
    anchorRef.current = null;
    infoAnchorRef.current = null;
    entryRef.current = null;
    programmaticRef.current = false;
    setFollowing(false);
  }, [identityKey]);

  // 首次成功 ready 之后贴底一次；此后不抢用户的阅读位置。
  const initialisedRef = useRef<string | null>(null);
  useEffect(() => {
    if (!ready) return;
    if (initialisedRef.current === identityKey) return;
    initialisedRef.current = identityKey;

    markProgrammatic(() => stick.scrollToBottom());
    setFollowing(true);
    anchorRef.current = readAnchor();
  }, [identityKey, markProgrammatic, readAnchor, ready, stick]);

  const followIfAtBottom = useCallback(() => {
    stick.followIfAtBottom();
    if (stick.isFollowing()) setFollowing(true);
  }, [stick]);

  const scrollToBottom = useCallback(() => {
    markProgrammatic(() => stick.scrollToBottom());
    setFollowing(true);
    anchorRef.current = readAnchor();
  }, [markProgrammatic, readAnchor, stick]);

  const preserve = useCallback((change: () => void) => {
    const anchor = readAnchor();
    anchorRef.current = anchor;
    // 先放弃跟随，否则恢复之后的下一个 delta 会把人拽回底部。
    stick.releaseFollow();
    setFollowing(false);
    change();
    // 布局已经落定（React 同步刷新 DOM 后），此刻再补偿。
    markProgrammatic(() => restoreAnchor(anchor));
    anchorRef.current = readAnchor();
  }, [markProgrammatic, readAnchor, restoreAnchor, stick]);

  const showTaskInfo = useCallback((reveal: () => void, entry: HTMLButtonElement) => {
    infoAnchorRef.current = readAnchor();
    entryRef.current = entry;
    stick.releaseFollow();
    setFollowing(false);
    reveal();
    const el = stick.ref.current;
    const info = taskInfoRef.current;
    if (el && info) {
      // 只滚本栏：`scrollIntoView` 会把外层主对话一起带走。
      el.scrollTop = info.offsetTop;
    }
    taskInfoHeadingRef.current?.focus({ preventScroll: true });
  }, [readAnchor, stick]);

  const restoreTaskInfo = useCallback((hide: () => void) => {
    const anchor = infoAnchorRef.current;
    infoAnchorRef.current = null;
    const entry = entryRef.current;
    entryRef.current = null;
    hide();
    markProgrammatic(() => restoreAnchor(anchor));
    // 只有焦点真的落在被卸载的信息区里时才还回去，否则会抢走别处的焦点。
    const active = document.activeElement;
    const insideInfo = active instanceof Node && taskInfoHeadingRef.current?.contains(active);
    if (insideInfo || active === taskInfoHeadingRef.current || active === document.body) {
      if (entry?.isConnected) entry.focus({ preventScroll: true });
      else onFallbackFocus();
    }
    anchorRef.current = readAnchor();
  }, [markProgrammatic, onFallbackFocus, readAnchor, restoreAnchor]);

  return {
    ref: stick.ref,
    contentRef,
    taskInfoRef,
    taskInfoHeadingRef,
    following,
    followIfAtBottom,
    scrollToBottom,
    preserve,
    showTaskInfo,
    restoreTaskInfo,
    identityKey,
  };
}
