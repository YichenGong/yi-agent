/**
 * 槽位标识与事件名。
 *
 * **必须与 Rust 侧 `bridge.rs` 的 `event_name` 保持一致**：那里是
 * `format!("app-server://{kind}/{}", slot.as_str())`，本地固定 `"local"`。
 * 两边各写一遍是既有架构的既成事实（前端不 import Rust），所以这里只做一件事：
 * 把它变成唯一常量，别处不再出现裸字符串。
 */

/** 本地 sidecar 的固定槽位名，与 `ProfileKey::Local.as_str()` 对齐。 */
export const LOCAL_SLOT = "local";

/** 远端槽位名即 `remote.json` 里的 profile id。 */
export type Slot = string;

export type EventKind = "message" | "request" | "status";

export function eventName(kind: EventKind, slot: Slot): string {
  return `app-server://${kind}/${slot}`;
}
