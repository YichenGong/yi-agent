/**
 * 最后选中的会话 id，用于冷启动复位。
 *
 * 与 `theme.ts` 同一条契约：存储只是缓存，**从不抛**，坏值一律当没有。它只有
 * 一个用途——下次启动时把手放回那条会话；会话本身是否存在由服务端列表裁决
 * （见 `App` 握手里的回落分支），这里不校验。
 *
 * 为什么不复用 `threadListCache.selectedId`：那份快照的形状被解析器与测试逐字段
 * 钉住，且它记的是"上次列表刷新那一刻"的选中项，与本键"最后看的"不是同一件事。
 */
import type { StorageLike } from "./remoteConfig";

export const SELECTED_THREAD_KEY = "yi-agent.selectedThread";

export function readSelectedThread(
  storage: StorageLike | undefined = globalThis.localStorage as StorageLike | undefined,
): string | null {
  if (!storage) return null;
  try {
    const raw = storage.getItem(SELECTED_THREAD_KEY);
    return raw !== null && raw.length > 0 ? raw : null;
  } catch {
    return null;
  }
}

export function writeSelectedThread(
  storage: StorageLike | undefined,
  id: string | null,
): void {
  if (!storage) return;
  try {
    if (id === null) storage.removeItem(SELECTED_THREAD_KEY);
    else storage.setItem(SELECTED_THREAD_KEY, id);
  } catch {
    // 配额 / 隐私模式：丢一条缓存不值得打断用户切会话。
  }
}
