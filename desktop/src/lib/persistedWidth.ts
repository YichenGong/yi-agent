/** 一条持久化的像素宽度（右栏 / 侧栏各一条），存 localStorage。 */
export interface PersistedWidthSpec {
  storageKey: string;
  defaultPx: number;
  minPx: number;
  maxPx: number;
}

export interface PersistedWidth {
  clamp(px: number): number;
  load(): number;
  save(px: number): void;
}

export function createPersistedWidth(spec: PersistedWidthSpec): PersistedWidth {
  const clamp = (px: number): number => {
    if (!Number.isFinite(px)) return spec.defaultPx;
    return Math.min(spec.maxPx, Math.max(spec.minPx, px));
  };
  return {
    clamp,
    load(): number {
      try {
        const raw = localStorage.getItem(spec.storageKey);
        if (raw === null) return spec.defaultPx;
        return clamp(parseFloat(raw));
      } catch {
        // localStorage can throw in restricted contexts; never let that break rendering.
        return spec.defaultPx;
      }
    },
    save(px: number): void {
      try {
        localStorage.setItem(spec.storageKey, String(clamp(px)));
      } catch {
        // 存储不可用（隐私模式等）时静默降级：宽度只是偏好。
      }
    },
  };
}
