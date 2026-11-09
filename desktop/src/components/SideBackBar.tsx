/**
 * 旁支视图顶部的「返回主会话」条。
 *
 * 为什么需要它：旁支曾经只有 `/back` 这一个出口，提示只写在
 * `MessageInput` 的 placeholder 与状态栏文本里——placeholder 一被用户打字就
 * 盖住，GUI 上等于没有出口。用户报的「没有返回按钮」就是这条。
 *
 * 为什么长得像侧栏的「+ New thread」：那颗按钮是这套界面里「一条主操作」的
 * 既有形状（`ThreadSidebar` 顶部，宽 = 侧栏宽度，`bg-raised` 无边框，左对齐
 * 单行文字）。返回是同一层级的动作，用同一套形状，用户不必为它重新学一种视觉
 * 语言。因此宽度**跟着侧栏实时变**（订阅 `publishSidebarWidth`），而不是写死
 * 常量或铺满整列——铺满会让它读成一条新的系统栏，正是 2026-11-05 规格 §4.1
 * 要避免的东西。
 *
 * 它**只**表达一件事：退出旁支、回到父会话。退出就是销毁：这一轮回答会被
 * 取消，本次旁问的正文、草稿与用量随之丢弃，下次进入是全新环境
 * （spec §4.3）。/back 与 /side-close 是同一件事，没有「返回但保留」。
 */
import { useSyncExternalStore } from "react";
import {
  currentSidebarWidth,
  subscribeSidebarWidth,
} from "../lib/sidebarWidth";

export function SideBackBar({
  parentTitle,
  onBack,
}: {
  /** 父会话标题；未知（列表还没到）时传 `null`，此时只显示动作本身。 */
  parentTitle: string | null;
  /**
   * 退出旁支、回到父视图。
   *
   * 本地视图立刻切走（不等网络），并补发一次精确的 `side/exit` 请宿主销毁
   * 这一代；发不出去就留成待补发的退出意图，不会把正文带回来。
   */
  onBack: () => void;
}) {
  // 与侧栏同一份数值、同一时刻：拖动分隔条时两边一起变宽变窄。
  const width = useSyncExternalStore(subscribeSidebarWidth, currentSidebarWidth);

  return (
    // 外层只有水平内边距，和侧栏把手那颗「+ New thread」的 `p-2` 对齐；
    // `shrink-0` 保证它不会在正文很长时被挤扁。
    <div className="shrink-0 px-2 pt-2">
      <button
        type="button"
        // 无障碍名只用动作本身，不含父标题：读屏念出「返回主会话：某个标题」
        // 太长，而标题在正文里已经可见。`title` 里补全文供悬停查看。
        aria-label="返回主会话"
        title={
          parentTitle
            ? `退出旁问并回到主会话：${parentTitle}`
            : "退出旁问并回到主会话"
        }
        onClick={onBack}
        // `maxWidth: 100%` 只是兜底：窗口窄到主列比侧栏还窄时，宁可让它略窄
        // 也不能溢出把正文挤出视口。
        style={{ width, maxWidth: "100%" }}
        // 与「+ New thread」逐项对齐：同样的圆角、底色、内边距与字号。
        // 唯一差别是文字颜色——返回是次要动作，用 `text-fg-muted` 让它比主
        // 操作退后半步，但用户在旁支里一眼还是能看到它。
        className="flex items-center gap-2 rounded-md bg-raised px-3 py-2 text-left text-sm text-fg-muted hover:bg-raised/80 hover:text-fg"
      >
        {/*
         * 箭头是装饰：读屏只念 `aria-label`（整条按钮），视觉用户看到箭头
         * 才有「往回走」的方向感，故 `aria-hidden`。
         */}
        <span aria-hidden="true" className="shrink-0 text-base leading-none">
          ←
        </span>
        <span className="shrink-0">返回主会话</span>
        {parentTitle ? (
          // 父标题可能很长：截断而不是把按钮撑开，`title` 里保留全文。
          <span className="min-w-0 truncate text-xs text-fg-subtle">
            {parentTitle}
          </span>
        ) : null}
      </button>
    </div>
  );
}
