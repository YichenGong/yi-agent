/** Per-thread autonomy mode, mirroring the Rust `ThreadMode` (serde lowercase). */
export type ThreadMode = "normal" | "yolo";

/** params for `thread/setPermissionMode`. Note the camelCase key (`threadId`)
 *  matches the server's param parsing; list responses use snake_case. */
export function setPermissionModeParams(threadId: string, mode: ThreadMode) {
  return { threadId, mode };
}
