/** Per-thread autonomy mode, mirroring the Rust `ThreadMode` (serde lowercase). */
export type ThreadMode = "normal" | "yolo";

/** params for `thread/setPermissionMode`. Note the camelCase key (`threadId`)
 *  matches the server's param parsing; list responses use snake_case. */
export function setPermissionModeParams(threadId: string, mode: ThreadMode) {
  return { threadId, mode };
}

/**
 * 从 `thread/resume` / `thread/start` 的响应里读权威权限模式。
 *
 * 响应现在直接带 `permission_mode`（此前只能从 `thread/listAll` 快照回读，
 * 那条路径异步、可失败、可与另一端写入竞态——「手机上打开会话时 YOLO 概率性
 * 没同步过来」的成因）。缺失或非法一律返回 `null`：未知，**不得**回退成 normal。
 */
export function permissionModeFromResponse(r: unknown): ThreadMode | null {
  const mode = (r as { permission_mode?: unknown } | null | undefined)?.permission_mode;
  return mode === "normal" || mode === "yolo" ? mode : null;
}
