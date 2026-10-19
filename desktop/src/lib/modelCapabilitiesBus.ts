/**
 * `model/capabilitiesChanged` 的进程内广播。
 *
 * 为什么需要它：能力探测是宿主后台跑完的（最长 30s），结果以全局通知发出。而
 * 桌面端当前**没有**全局模型清单 store——`SettingsModelsTab` 与 `ModelPicker`
 * 各自持有一份 `catalog`。没有这条总线，探测结束后两个组件的下拉会一直停在
 * 「能力未知」的七档平铺状态，用户以为按钮没生效。
 *
 * 设计刻意做成最小面：只传「哪条模型变了」，订阅方自己去 `model/list` 重读。
 * 不在这里缓存清单，避免出现第二份真相。
 */
import type { ReasoningCapabilities } from "./models";

export type CapabilityChange = {
  name: string;
  capabilities: ReasoningCapabilities | null;
};

type Listener = (change: CapabilityChange) => void;

const listeners = new Set<Listener>();

/** 订阅能力变更；返回取消订阅函数。 */
export function onModelCapabilitiesChanged(listener: Listener): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** 通知所有订阅方。宿主推来一帧就调一次。 */
export function notifyModelCapabilitiesChanged(change: CapabilityChange): void {
  for (const listener of listeners) {
    try {
      listener(change);
    } catch {
      // 一个订阅方抛错不该掐断其它人：这只是一次「该刷新了」的提示。
    }
  }
}

/** 测试用：清空订阅，避免用例之间互相串台。 */
export function resetModelCapabilitiesListeners(): void {
  listeners.clear();
}
