import { useEffect, useRef, useState, type RefObject } from "react";

/**
 * Tauri 拖放事件的形状（`@tauri-apps/api/webview` 的 `DragDropEvent`）。
 *
 * 这里自己声明一份而不是 import 那个类型：远端（iOS）构建不该因为类型引用而把
 * `@tauri-apps/api` 拉进静态图（本仓库一律**动态** import 它，见 `App.tsx`）。
 * 结构上与真类型一致，`App` 里那一层 subscribe 直接把它转发进来。
 */
export type DragDropPosition = { x: number; y: number };

export type DragDropPayload =
  | { type: "enter"; paths: string[]; position: DragDropPosition }
  | { type: "over"; position: DragDropPosition }
  | { type: "drop"; paths: string[]; position: DragDropPosition }
  | { type: "leave" };

/** 注入的订阅接缝：返回的 Promise 解析为 `unlisten`。 */
export type DragDropSubscribe = (handler: (event: DragDropPayload) => void) => Promise<() => void>;

/**
 * 拖拽点是否落在 `el` 的矩形内。
 *
 * `position` 来自 Tauri，是**物理像素**；`getBoundingClientRect()` 是 CSS 像素。
 * 必须除以 `devicePixelRatio`（Retina 上是 2，不除会让判定整体偏移约一倍）。
 * 边界取「左闭右开」：与 `elementFromPoint` 一类的命中语义一致，且矩形相邻时
 * 不会出现一个点同时命中两个框。
 */
export function dragHitsTarget(
  el: HTMLElement | null,
  position: DragDropPosition,
  ratio: number = globalThis.devicePixelRatio || 1,
): boolean {
  if (el === null) return false;
  const r = el.getBoundingClientRect();
  const scale = ratio > 0 ? ratio : 1;
  const x = position.x / scale;
  const y = position.y / scale;
  return x >= r.left && x < r.right && y >= r.top && y < r.bottom;
}

/**
 * 订阅窗口级拖放事件，判定落点是否在 `targetRef` 上，并给出高亮状态。
 *
 * 为什么挂在窗口级而不是把监听装在输入框上：拖放事件是窗口级的，而输入框在
 * 看板视图/无会话时不渲染——监听随组件装卸会漏掉 `leave` 一类的边界。这里订阅
 * 一次、整窗口有效，落点仍由 `targetRef` 的矩形决定。
 *
 * `isRemote` 为真（iOS/远端）时**完全不订阅**：那条路没有 Tauri webview API。
 * `subscribe` / `onDrop` / `targetRef` 都经 ref 读取，故调用方不必为每次渲染
 * 重建订阅（重建会漏事件）。
 */
export function useDragDropPaths({
  subscribe,
  isRemote,
  targetRef,
  onDrop,
}: {
  subscribe: DragDropSubscribe;
  isRemote: boolean;
  targetRef: RefObject<HTMLElement | null>;
  onDrop: (paths: string[]) => void;
}): { active: boolean } {
  const [active, setActive] = useState(false);
  const subscribeRef = useRef(subscribe);
  const onDropRef = useRef(onDrop);
  useEffect(() => {
    subscribeRef.current = subscribe;
    onDropRef.current = onDrop;
  }, [subscribe, onDrop]);

  useEffect(() => {
    if (isRemote) return;
    let alive = true;
    let unlisten: (() => void) | null = null;

    void subscribeRef
      .current((event) => {
        if (event.type === "enter" || event.type === "over") {
          setActive(dragHitsTarget(targetRef.current, event.position));
          return;
        }
        if (event.type === "leave") {
          setActive(false);
          return;
        }
        const hit = dragHitsTarget(targetRef.current, event.position);
        setActive(false);
        if (hit && event.paths.length > 0) onDropRef.current(event.paths);
      })
      .then((fn) => {
        // 订阅解析回来时组件可能已经卸载：那时直接摘掉，别把监听留在窗口上。
        if (alive) unlisten = fn;
        else fn();
      });

    return () => {
      alive = false;
      unlisten?.();
    };
  }, [isRemote, targetRef]);

  return { active };
}
