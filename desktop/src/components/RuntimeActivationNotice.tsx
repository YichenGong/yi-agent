export interface RuntimeActivationNoticeProps {
  local: boolean;
  failed: boolean;
  busy: boolean;
  onRetry: () => void;
}

/** A recovery action, never a request to send or replay the user's draft. */
export function RuntimeActivationNotice({
  local, failed, busy, onRetry,
}: RuntimeActivationNoticeProps) {
  if (!local || !failed) return null;
  return (
    <div className="runtime-startup-notice" role="status">
      <span className="runtime-startup-notice__text">
        {busy ? "正在恢复运行环境，草稿已保留" : "此会话的运行环境未就绪，历史和草稿已保留"}
      </span>
      <button type="button" className="runtime-startup-notice__retry"
        disabled={busy} onClick={onRetry}>
        准备运行环境并重试
      </button>
    </div>
  );
}
