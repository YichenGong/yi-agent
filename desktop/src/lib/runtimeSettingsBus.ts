/**
 * `settings/changed` 的进程内广播，按 **RpcClient 身份**隔离。
 *
 * 为什么需要它：设置页只在打开时读一次。别的窗口、手机、或者本进程的观察器改了
 * 文件之后，页面上显示的还是旧的，用户会以为保存没生效。
 *
 * 为什么带 client 参数：桌面端可以同时连着多个宿主（本机 + 远端项目）。以 client
 * 身份为键，远端项目 A 的保存就不会去刷新另一台主机 B 的设置页——那种串台会让
 * 用户在一个项目里看到另一个项目的值。
 *
 * 总线只传「失效」这个事实，**不缓存设置**：真正的值永远从 `settings/read` 拿，
 * 避免出现第二份真相。
 */
import type { RpcClient } from "./rpc";

type Listener = () => void;

/**
 * `WeakMap` 而不是 `Map`：client 被替换/丢弃时条目自然消失，不会把旧连接连同它的
 * 监听器一起挂在内存里。
 */
const listeners = new WeakMap<RpcClient, Set<Listener>>();

/** 订阅某台宿主的设置失效；返回取消订阅函数。 */
export function onRuntimeSettingsInvalidated(client: RpcClient, listener: Listener): () => void {
  let set = listeners.get(client);
  if (!set) {
    set = new Set();
    listeners.set(client, set);
  }
  set.add(listener);
  if (trackedClients.size < 32) trackedClients.add(client);
  return () => {
    const current = listeners.get(client);
    if (!current) return;
    current.delete(listener);
    // 最后一个订阅者退出就清掉条目：留一个空 Set 也不错，但清掉能保证
    // 「注册新订阅者」永远从干净状态开始。
    if (current.size === 0) listeners.delete(client);
  };
}

/** 告诉所有关心这台宿主的订阅方「去重读」。 */
export function notifyRuntimeSettingsInvalidated(client: RpcClient): void {
  const set = listeners.get(client);
  if (!set) return;
  for (const listener of [...set]) {
    try {
      listener();
    } catch {
      // 一个订阅方抛错不该掐断其它人：这只是一次「该刷新了」的提示。
    }
  }
}

/** 测试用：清空订阅，避免用例之间互相串台。 */
export function resetRuntimeSettingsBus(): void {
  // WeakMap 没有 clear()；测试里 client 都是模块级常量，需要显式清。
  for (const client of trackedClients) listeners.delete(client);
  trackedClients.clear();
}

/**
 * 测试辅助：`WeakMap` 无法枚举，所以额外记一份引用。
 *
 * 只在 `resetRuntimeSettingsBus` 里使用，生产路径不碰它——一份强引用列表会阻止
 * 回收，那就违背了选 WeakMap 的理由。
 */
const trackedClients = new Set<RpcClient>();
