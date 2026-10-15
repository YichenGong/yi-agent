import { useLayoutEffect } from "react";
import { anchorSpin } from "./spinPhase";

/**
 * 让 `ref` 指向的元素持续旋转，且旋转相位锚定到**墙钟**（见 `spinPhase.ts` 顶部
 * 的根因说明）。
 *
 * 依赖只有 `[ref, active]`：状态在 running ↔ awaiting_approval 之间翻转时 `active`
 * 恒为 true，effect 不重跑、`Animation` 实例不变，时间轴连续；只有当元素真的离开
 * 或回来（idle ↔ 非 idle）才重建，而重建时又按墙钟相位挂号，所以视觉上无缝。
 *
 * 用 `useLayoutEffect`（而非 `useEffect`）是为了在首次绘制**之前**就把相位挂好，
 * 避免闪一帧 0deg。
 */
export function useSpinPhase(
  ref: { readonly current: Element | null },
  active: boolean,
): void {
  useLayoutEffect(() => {
    if (!active) return;
    const el = ref.current;
    if (!el) return;
    const anim = anchorSpin(el, performance.now());
    if (!anim) return;
    return () => anim.cancel();
  }, [ref, active]);
}