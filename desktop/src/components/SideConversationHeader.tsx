import type { SideStatusWire } from "../lib/generated/SideStatusWire";
import type { SideSummaryWire } from "../lib/generated/SideSummaryWire";

/**
 * 旁支视图的顶栏。
 *
 * 三条信息必须同时可见，因为它们决定用户敢不敢继续用这条旁支：
 * - **它是旁支**（"旁问 · 基于创建时上下文"）——旁支的上下文是创建那一刻的快照，
 *   父会话之后发生的事它都不知道。不写明，用户会以为在跟同一个会话说话。
 * - **父现在在干什么**（父状态是回父链接，不是旁支状态）。
 * - **只读**——旁支不能改文件、不能跑命令，只能回答问题。
 *
 * 顶栏的「主任务等待审批」是**回父**的链接，不是旁支的审批入口：父 approval 的
 * id 属于父会话，交给旁支处理会让一次批准落到错误的线程上。
 */
export interface SideConversationHeaderProps {
  summary: SideSummaryWire;
  /** 父会话此刻的状态文案（`running` / `awaiting_approval` / `idle` …）。 */
  parentStatus: string;
  /** 本设备能否下发 Control 操作（Observe 不能）。 */
  canControl: boolean;
  onBack: () => void;
  /** 缺省 `null`：Observe 端没有关闭旁支的能力，按钮不渲染而不是禁用。 */
  onClose?: (() => void) | null;
  /** 旁支自己在跑时可 Stop；缺省 `null` 表示不渲染。 */
  onStop?: (() => void) | null;
  /** 关闭请求在途时禁掉关闭按钮，避免二次确认叠两层。 */
  closing?: boolean;
}

const STATUS_TEXT: Record<SideStatusWire, string> = {
  creating: "正在建立",
  idle: "空闲",
  running: "正在回答",
  failed: "已失败",
  closing: "正在关闭",
  closed: "已关闭",
};

export function SideConversationHeader({
  summary,
  parentStatus,
  canControl,
  onBack,
  onClose = null,
  onStop = null,
  closing = false,
}: SideConversationHeaderProps) {
  const running = summary.status === "running";
  return (
    <div
      className="flex flex-wrap items-center gap-2 border-b border-line bg-raised px-3 py-2 text-xs"
      data-testid="side-header"
    >
      <span className="font-medium text-fg">旁问</span>
      <span className="text-fg-muted">· 基于创建时上下文</span>
      <span className="rounded border border-line px-1 text-fg-muted">只读</span>
      <span className="text-fg-muted">{STATUS_TEXT[summary.status]}</span>
      {/* 父状态是「回去的入口」：点它只切回主会话，绝不对旁支做父操作。 */}
      <button
        type="button"
        onClick={onBack}
        className="rounded px-1 text-fg-muted underline-offset-2 hover:text-fg hover:underline"
        title="回到主会话"
      >
        父：{parentStatus}
      </button>
      {summary.expires_at !== null && summary.status !== "closed" && (
        <span className="text-fg-muted">
          {formatExpiry(summary.expires_at)}
        </span>
      )}
      <span className="max-md:ml-auto" />
      {running && onStop !== null && (
        <button
          type="button"
          onClick={onStop}
          className="rounded-md bg-red-600 px-3 py-1 font-medium text-white hover:bg-red-500"
        >
          停止旁问
        </button>
      )}
      <button
        type="button"
        onClick={onBack}
        className="rounded-md border border-line-strong px-3 py-1 text-fg-muted hover:bg-panel hover:text-fg"
      >
        返回主会话
      </button>
      {canControl && onClose !== null && !summary.stopping && (
        <button
          type="button"
          onClick={onClose}
          disabled={closing}
          className="rounded-md border border-line-strong px-3 py-1 text-fg-muted hover:bg-panel hover:text-fg disabled:opacity-50"
        >
          关闭旁问
        </button>
      )}
    </div>
  );
}

/** 剩余有效期的一句话。已过期也照实说，不假装还在。 */
function formatExpiry(expiresAt: number): string {
  const remaining = expiresAt - Date.now();
  if (remaining <= 0) return "已过期";
  const minutes = Math.ceil(remaining / 60_000);
  return `${minutes} 分钟后自动清理`;
}
