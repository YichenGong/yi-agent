import type { DragDropPayload, DragDropSubscribe } from "../hooks/useDragDropPaths";

/**
 * 真实的 Tauri 拖放订阅（桌面端唯一落点）。
 *
 * 单独成文件是为了让 `App` 与测试都只依赖一个稳定符号：`@tauri-apps/api` 一律
 * **动态** import（本仓库既有做法，见 `App.tsx` 的 `core` / `plugin-dialog`），
 * 远端构建因此不会把它拉进静态图。测试直接在模块边界 mock 掉它。
 */
export const subscribeDragDrop: DragDropSubscribe = async (handler) => {
  const { getCurrentWebview } = await import("@tauri-apps/api/webview");
  // Tauri 交来的是事件对象（`{ payload }`），hook 只认 payload 本身。
  return getCurrentWebview().onDragDropEvent((event) =>
    handler(event.payload as DragDropPayload),
  );
};
