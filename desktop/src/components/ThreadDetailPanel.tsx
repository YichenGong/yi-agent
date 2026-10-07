/**
 * 主对话下方的常驻详情面板：轨迹与 Git Diff 两个 Tab。
 *
 * 面板常驻（而非只在选中子 agent 时出现），因为 Git Diff 是会话级的、与选了谁无关；
 * 「轨迹」Tab 在未选子 agent 时给空态。Tab 可点，也可由外部（点卡片 / 模型推送）
 * 切换：`tab` 是受控 prop，切换由 `onTabChange` 上抛。
 *
 * 页眉还承担一个与 Tab 无关的职责：子 agent 栏收起后把它放回来。状态栏那颗图标
 * 现在管的是本面板的开合，栏自身的「收起」只会单向隐藏栏——没有这里这颗「展开子
 * agent 栏」，收起就再也回不去了。故它只在栏确实收起时渲染。
 */
import { SubagentTrace } from "./SubagentTrace";
import { GitDiffView } from "./GitDiffView";
import type { ComponentProps } from "react";

type Tab = "trace" | "diff";

export function ThreadDetailPanel({
  tab,
  onTabChange,
  onClose,
  traceProps,
  diffProps,
  railCollapsed,
  onExpandRail,
}: {
  tab: Tab;
  onTabChange: (t: Tab) => void;
  onClose: () => void;
  traceProps: ComponentProps<typeof SubagentTrace> | null;
  diffProps: ComponentProps<typeof GitDiffView>;
  /** 子 agent 栏是否已收起；收起时页眉提供重新展开的入口。 */
  railCollapsed: boolean;
  onExpandRail: () => void;
}) {
  const tabClass = (t: Tab) =>
    `rounded px-2 py-0.5 text-xs ${
      tab === t ? "bg-panel text-fg" : "text-fg-muted hover:text-fg"
    }`;
  return (
    <section
      aria-label="会话详情"
      className="flex min-h-0 flex-1 flex-col border-t border-line bg-surface"
    >
      <div className="flex items-center justify-between px-3 py-1.5">
        <div role="tablist" className="flex items-center gap-1">
          <button type="button" role="tab" aria-selected={tab === "trace"} className={tabClass("trace")} onClick={() => onTabChange("trace")}>
            轨迹
          </button>
          <button type="button" role="tab" aria-selected={tab === "diff"} className={tabClass("diff")} onClick={() => onTabChange("diff")}>
            Git Diff
          </button>
        </div>
        <div className="flex items-center gap-2">
          {railCollapsed && (
            <button
              type="button"
              className="text-xs text-fg-subtle hover:text-fg-muted"
              onClick={onExpandRail}
            >
              展开子 agent 栏
            </button>
          )}
          <button type="button" aria-label="关闭详情" className="text-xs text-fg-subtle hover:text-fg-muted" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>

      <div className="flex min-h-0 flex-1 flex-col">
        {tab === "trace" ? (
          traceProps ? (
            <SubagentTrace {...traceProps} />
          ) : (
            <p className="px-3 py-2 text-sm text-fg-subtle">未选择子 agent</p>
          )
        ) : (
          <GitDiffView {...diffProps} />
        )}
      </div>
    </section>
  );
}
