/**
 * 一条通知该不该写进当前显示的对话视图。
 *
 * 为什么需要这一步（spec §4.6 判据 17）：本机可以同时连 N 条槽位——本地一条、
 * 每个远端项目各一条——而它们共用**同一个** `ThreadStore`。thread id 在各
 * app-server 之间没有任何全局唯一性保证：两个项目里都叫 `t-1` 完全可能。
 * 不做归属判定，A 项目跑任务时 B 项目的视图会跟着动，两条对话的内容被拼进
 * 同一份视图。
 *
 * 判定只作用于**视图写入**，不影响连接本身：非活动槽位照常收帧、照常握手，
 * 否则切到它时会发现连接已经半死。
 */

/**
 * 与槽位无关的全局通知。
 *
 * 这些描述的是「这台 App 的显示/能力」，不归属任何一条会话，所以无论哪个槽位
 * 送来都应该生效——把它们也挡掉，会出现「用户在远端项目里改了主题，本地视图
 * 不跟着变」这种莫名其妙的割裂。
 */
const SLOT_AGNOSTIC = new Set([
  "ui/settings/updated",
  "model/capabilitiesChanged",
]);

/** `slot` 送来的 `method` 通知，是否应写入当前（`activeSlot`）视图。 */
export function ownsThreadView(
  slot: string,
  activeSlot: string,
  method: string,
): boolean {
  if (SLOT_AGNOSTIC.has(method)) return true;
  return slot === activeSlot;
}
