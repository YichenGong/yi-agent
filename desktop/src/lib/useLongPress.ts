import { useCallback, useEffect, useRef } from "react";
import type { PointerEvent as ReactPointerEvent } from "react";
import { PRESS_IGNORE_SELECTOR } from "./sidebarPress";

/** 按住多久算长按。 */
export const LONG_PRESS_MS = 500;
/** 手指允许漂移多少像素；超过即认定为滚动/拖动，取消长按。 */
export const MOVE_TOLERANCE_PX = 10;

/**
 * 「吞 click」要放过的落点：长按刚开出的菜单本身。
 *
 * 菜单（面板与那层透明 backdrop）渲染在被长按的行**内部**，而行自身有 onClick
 * （选中会话）。吞 click 若不放过菜单，点菜单项会连自己一起被吞 → 三项全部点不动；
 * 点 backdrop 也关不掉菜单。故按标记区分「行自身的点击」与「菜单内的点击」。
 */
const SWALLOW_IGNORE_SELECTOR = "[data-thread-menu], [data-thread-menu-overlay]";
/** 吞掉监听的自摘时限：久到够覆盖那一次合成 click，又不至于吃掉下一次真点击。 */
const SWALLOW_WINDOW_MS = 400;

/** 长按落点：会话行或工作区分组头（文件夹）。 */
export type LongPressTarget =
  | { kind: "thread"; id: string; el: HTMLElement }
  | { kind: "group"; ws: string; el: HTMLElement };

/** 由调用方把 pointerdown 的目标解析成落点；返回 null 表示这次按下不算长按。 */
export type ResolveTarget = (e: ReactPointerEvent<HTMLElement>) => LongPressTarget | null;

export interface LongPressOptions {
  /** false 时完全不装监听（桌面端走这条）。 */
  enabled: boolean;
  resolveTarget: ResolveTarget;
  onLongPress: (target: LongPressTarget) => void;
  thresholdMs?: number;
  moveTolerancePx?: number;
}

/**
 * 长按识别，委托式：挂在侧栏容器上，一份监听管住所有会话行与组头。
 *
 * 为什么是委托而不是每行一个 hook：行由 `renderThread` 在 `.map()` 里渲染，
 * 组数随数据变化，行内调 hook 会违反 Hooks 规则（渲染间的 hook 数量必须稳定）。
 * 委托把唯一的 hook 钉在组件顶层，落点交给 `resolveTarget` 判定。
 *
 * 为什么不用 `contextmenu`：iOS 的 WKWebView 长按**不派发**它（弹的是文字选择
 * callout），Android 则派发——同一份代码两端行为不一致。Pointer Events 两端一致。
 *
 * 为什么触发后要吞 click：长按会话行的同时，手指抬起会补一次 click，那会走
 * `onSelect` → `thread/resume`。吞掉它，长按才是「打开菜单」而不是「选中会话」。
 * 吞 click 必须用**捕获阶段**：React 19 的事件委托挂在根上，Bubble 阶段在元素上的
 * 监听会被先注册的同元素监听抢先，且 stopPropagation 撤不回已经发生的调用。
 *
 * 为什么不 `preventDefault` pointerdown：那会连抽屉的滚动一起废掉。滚动改由
 * `pointercancel`（系统接管手势时派发）与位移阈值取消。
 */
export function useLongPress({
  enabled,
  resolveTarget,
  onLongPress,
  thresholdMs = LONG_PRESS_MS,
  moveTolerancePx = MOVE_TOLERANCE_PX,
}: LongPressOptions): { onPointerDown: (e: ReactPointerEvent<HTMLElement>) => void } {
  // 回调放 ref：事件监听在 pointerdown 时一次性注册，闭包不应随每次渲染重建。
  const cb = useRef(onLongPress);
  const resolve = useRef(resolveTarget);
  useEffect(() => {
    cb.current = onLongPress;
    resolve.current = resolveTarget;
  }, [onLongPress, resolveTarget]);

  const timer = useRef<number | null>(null);
  const origin = useRef<{ x: number; y: number } | null>(null);
  const activeId = useRef<number | null>(null);
  const disposers = useRef<(() => void)[]>([]);
  const swallowCleanup = useRef<(() => void) | null>(null);

  /** 收尾：清计时器、摘监听、复位状态。幂等，可重复调用。 */
  const end = useCallback(() => {
    if (timer.current !== null) {
      window.clearTimeout(timer.current);
      timer.current = null;
    }
    origin.current = null;
    activeId.current = null;
    for (const dispose of disposers.current) dispose();
    disposers.current = [];
  }, []);

  // 卸载时务必清干净：否则组件没了，window 上还挂着监听和一个待触发的计时器；
  // 行上那个吞 click 的捕获监听与被长按元素的引用也要一并撤掉。
  useEffect(() => {
    return () => {
      end();
      swallowCleanup.current?.();
    };
  }, [end]);

  const onPointerDown = (e: ReactPointerEvent<HTMLElement>) => {
    if (!enabled) return;
    // 只认主按钮（鼠标左键 / 单指）。
    if (e.button !== 0) return;
    // 已有一根手指在计时：交给「第二指针」处理，不重复起手势。
    if (activeId.current !== null) return;
    const target = resolve.current(e);
    if (target === null) return;
    const el = target.el;

    const id = e.pointerId;
    activeId.current = id;
    origin.current = { x: e.clientX, y: e.clientY };

    const onMove = (ev: PointerEvent) => {
      if (ev.pointerId !== activeId.current) return;
      const o = origin.current;
      if (!o) return;
      if (Math.hypot(ev.clientX - o.x, ev.clientY - o.y) > moveTolerancePx) end();
    };
    const onRelease = (ev: PointerEvent) => {
      if (ev.pointerId === activeId.current) end();
    };
    const onSecondPointer = (ev: PointerEvent) => {
      if (ev.pointerId !== id) end();
    };

    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onRelease);
    window.addEventListener("pointercancel", onRelease);
    window.addEventListener("pointerdown", onSecondPointer);
    disposers.current.push(() => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onRelease);
      window.removeEventListener("pointercancel", onRelease);
      window.removeEventListener("pointerdown", onSecondPointer);
    });

    timer.current = window.setTimeout(() => {
      timer.current = null;
      const stillActive = activeId.current === id;
      end();
      if (!stillActive) return;

      // 上一次的吞 click 若还挂着，先摘掉，避免在多行间叠加。
      swallowCleanup.current?.();

      const arm = () => {
        let swallowTimeout: number | null = null;
        const dis = () => {
          el.removeEventListener("click", swallow, true);
          if (swallowTimeout !== null) window.clearTimeout(swallowTimeout);
          swallowTimeout = null;
          if (swallowCleanup.current === dis) swallowCleanup.current = null;
        };
        // 捕获阶段：必须跑在该元素自身（以及 React 根委托）的 click 监听之前，
        // 才能真正拦下这次 click。放过菜单内的点击（见 SWALLOW_IGNORE_SELECTOR）。
        const swallow = (ev: Event) => {
          const t = ev.target as Element | null;
          if (t?.closest?.(SWALLOW_IGNORE_SELECTOR)) return;
          ev.stopPropagation();
          ev.preventDefault();
          dis();
        };
        el.addEventListener("click", swallow, true);
        swallowTimeout = window.setTimeout(dis, SWALLOW_WINDOW_MS);
        swallowCleanup.current = dis;
      };
      arm();

      cb.current(target);
    }, thresholdMs);
  };

  return { onPointerDown };
}

export { PRESS_IGNORE_SELECTOR };
