/**
 * 状态栏那行连接状态的中文说法。
 *
 * 状态位里放的是传输层原值（`connecting` / `exited` / `connected`）或 App 拼的
 * `error: <message>`；原样显示就是一句英文，手机端底栏尤其显眼。这里只做「给人
 * 看的那一面」：原文仍由 `title` 保留，排查时把鼠标放上去就能看到。
 *
 * 与 `connectionError.ts` 分开：那边是**一次失败**的文案（会进对话红条），这边是
 * **当前连接态**的标签（常驻底栏），两者的生命周期和位置都不同。
 */

/** 传输层已知状态的显示名。 */
const LABELS: Record<string, string> = {
  connected: "已连接",
  connecting: "正在连接…",
  exited: "已断开，正在重连…",
};

/** 握手失败的统一说法：具体原因太长太技术，且下一轮重试就会换掉。 */
export const CONNECTION_FAILED_LABEL = "连接失败，正在重试…";

/** 把状态位翻成给人看的一行；未知状态原样返回，免得吞掉新状态。 */
export function connectionStatusLabel(status: string): string {
  if (status.startsWith("error:")) return CONNECTION_FAILED_LABEL;
  return LABELS[status] ?? status;
}
