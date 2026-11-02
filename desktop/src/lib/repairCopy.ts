/**
 * 凭据被服务端明确拒绝（直连 4401）时的文案；与「配对码无效」区分开。
 *
 * 单独成模块是因为它同时被 `App.tsx`（状态回调里落盘）与
 * `hooks/useNotificationRouter.ts`（`onStatus` 收到 4401 时落盘）使用。
 * 留在 `App.tsx` 里会让 hook 反向 import 组件，形成环。
 */
export const REPAIR_REMOVED_TEXT = "此设备已被桌面移除，请重新配对";
