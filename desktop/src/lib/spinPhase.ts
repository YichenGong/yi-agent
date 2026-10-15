/**
 * 侧栏「运行中」指示器的旋转相位。
 *
 * ## 为什么需要它
 *
 * 指示器要看起来「一直在转」，可它的旋转一直由 CSS 动画（Tailwind 的
 * `animate-spin`）驱动，而 **CSS 动画在元素被移动、或从文档移除后再插入时会从头
 * 重放**（`currentTime` 归零）。React 对带 `key` 的列表做重排时用的正是「移动 DOM
 * 节点」（对已挂载节点 `insertBefore`），于是只要侧栏列表顺序一变——服务端按
 * `updated_at` 降序返回会话，任何 `updated_at` 被刷新的会话（别的会话跑完一轮、
 * 改名、换模型……）都会让行重新排序——**正在运行那一行的旋转环就跳回 12 点**，
 * 即用户看到的「回退」。
 *
 * 关键点：节点的「身份不变」救不了它。同一节点被移动照样重启动画（实测
 * `identityChanges = 0`，时间轴却归零、`animationstart` 再次触发）。所以上一版
 * 「两态共用同一个 `<span>`、`animate-spin` 常驻」只修好了「状态翻转」（running →
 * awaiting_approval → running，同节点不移动），修不了「行重排」（移动节点）。
 *
 * ## 修法
 *
 * 不再用会随节点生命周期重启的 CSS 动画，改用 **Web Animations API**
 * （`element.animate`）：它挂在元素上，**不随 DOM 移动重启**（Chrome 与 WKWebView
 * 实测：同一父节点内移动、以及移动祖先子树，`currentTime` 都连续推进）。再把它的
 * `currentTime` 锚定到**墙钟相位**（`now % 周期`）而不是 0——于是即便某次真的重挂
 * （例如会话从 idle 又变回 running），新动画也落在与全局一致的相位上，视觉上无缝。
 *
 * 不支持 WAAPI 的环境（如 jsdom、极老的 webview）由 `supportsWaapi()` 判定为
 * false，调用方回退到 CSS `animate-spin`（此时只是失去「防重排」这一层，仍能转）。
 */

/** 一整圈的时长（毫秒），与 Tailwind `animate-spin` 的 1s 一致。 */
export const SPIN_PERIOD_MS = 1000;

/** 墙钟时刻 `now` 落在的旋转相位（毫秒，恒在 `[0, SPIN_PERIOD_MS)`；负时钟取模后归正）。 */
export function spinPhaseMs(now: number): number {
  const phase = now % SPIN_PERIOD_MS;
  return phase < 0 ? phase + SPIN_PERIOD_MS : phase;
}

/** 一个整圈：`rotate(0deg) → rotate(360deg)`。 */
export const SPIN_KEYFRAMES: Keyframe[] = [
  { transform: "rotate(0deg)" },
  { transform: "rotate(360deg)" },
];

/** 匀速、无限循环，与 CSS `animate-spin` 的手感一致。 */
export const SPIN_TIMING: KeyframeAnimationOptions = {
  duration: SPIN_PERIOD_MS,
  iterations: Infinity,
  easing: "linear",
};

/**
 * 当前环境是否支持 `element.animate`。为 false 时调用方保留 CSS `animate-spin`
 * 兜底（拿不到「防重排」，但至少有得转）。
 */
export function supportsWaapi(): boolean {
  return typeof Element !== "undefined" && typeof Element.prototype.animate === "function";
}

/**
 * 在 `el` 上起一个整圈旋转动画，并把相位锚到墙钟 `now`（而不是 0），返回该
 * `Animation` 供调用方在卸载时 `cancel()`。环境不支持 WAAPI 时返回 `null`。
 */
export function anchorSpin(el: Element, now: number): Animation | null {
  if (typeof el.animate !== "function") return null;
  const anim = el.animate(SPIN_KEYFRAMES, SPIN_TIMING);
  anim.currentTime = spinPhaseMs(now);
  return anim;
}