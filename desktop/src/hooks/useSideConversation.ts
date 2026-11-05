import { useEffect, useRef } from "react";
import type { RpcClient } from "../lib/rpc";
import type { Slot } from "../lib/slots";
import {
  SideConversationStore,
  isSideNotification,
  type SideHostBinding,
  type SideHostKey,
  type SideNotification,
  type SideView,
} from "../lib/sideConversation";
import { SideSyncController } from "../lib/sideSync";

/**
 * 把 `SideSyncController` 接进 React 的生命周期。
 *
 * 不做的事（都是刻意的）：
 * - **不**把旁支视图塞进 `ThreadStore`。旁支是临时的，它没有转录缓存、不进搜索、
 *   不占未读。任何「顺手复用普通会话管线」的写法都会让一段本该消失的内容留在
 *   磁盘里。
 * - **不**把父会话的 state 搬过来。父的 `currentId`/草稿/附件都不属于旁支。
 * - **不**在回调里读「当前」连接。每个控制器绑定一条具体的 `SideHostBinding`。
 *
 * 这里只做三件事：给旁支帧找对控制器、把可见性传给尾帧探测、卸载时收干净。
 */
export interface UseSideConversationDeps {
  store: SideConversationStore;
  /** 每次 store 变化后要求重渲染。 */
  onChange: () => void;
  /** 订阅父会话的回调（由 App 提供，走现有 parent subscribe 入口）。 */
  subscribeParent: (client: RpcClient, parentId: string) => Promise<void>;
  /**
   * 当前可见的旁支**身份**；`null` = 没有旁支视图在显示。
   *
   * 为什么不能只是一个 `parentId`：`parentId` 只在**一个宿主内**唯一，两个宿主
   * 可以各有 `t1`。按裸 id 同步可见性会把 B 宿主上 `t1` 的尾帧探测打开，同时
   * 让 A 的那条继续跑——两边的 `side/list` 都在发，而真正在屏幕上的一条可能
   * 根本没被探。
   */
  visible: { hostKey: SideHostKey; parentId: string } | null;
}

export interface SideConversationApi {
  /** 收到一条旁支帧。调用方已完成命名空间判别。 */
  accept: (client: RpcClient, slot: Slot, n: SideNotification) => void;
  /** 取（必要时创建）某宿主上某个父的控制器。 */
  controllerFor: (binding: SideHostBinding, parentId: string) => SideSyncController;
}

/** 控制器按 `(hostKey, parentId)` 归属：一个宿主可以有多个父各带一条旁支。 */
type ControllerKey = string;

function controllerKey(hostKey: SideHostKey, parentId: string): ControllerKey {
  return JSON.stringify([hostKey, parentId]);
}

export function useSideConversation(deps: UseSideConversationDeps): SideConversationApi {
  const { store, onChange, subscribeParent, visible } = deps;
  const controllers = useRef(new Map<ControllerKey, SideSyncController>());
  // 回调要看到最新的 props，但不能因为 props 变化就重建控制器（那会丢掉
  // 对账进度与缓存）。用 ref 影子保持「回调总是最新的」与「实例保持不变」两全。
  const latest = useRef({ onChange, subscribeParent, visible, store });
  latest.current = { onChange, subscribeParent, visible, store };

  const controllerFor = (
    binding: SideHostBinding,
    parentId: string,
  ): SideSyncController => {
    const key = controllerKey(binding.key, parentId);
    const existing = controllers.current.get(key);
    // 同槽位换了连接（重连）时 `key` 不变但 client 变了：旧控制器会继续往
    // 一条已 dispose 的客户端发请求。按客户端身份判定是否复用。
    if (existing && existing.client === binding.client) return existing;
    existing?.dispose();
    const next = new SideSyncController({
      binding,
      store: latest.current.store,
      subscribeParent: (id) => latest.current.subscribeParent(binding.client, id),
      clock: () => Date.now(),
      onChange: () => latest.current.onChange(),
    });
    controllers.current.set(key, next);
    // 新建的控制器必须**立刻**继承当前可见性。否则「重连后回到同一条旁支」
    // 会静默丢掉尾帧探测：可见性 effect 只在 `visible` 变化时跑，而这次变化
    // 发生在新建之前，于是新实例永远停在「不可见」，最后一帧丢了也没人对账。
    next.setVisible(
      latest.current.visible !== null &&
        latest.current.visible.hostKey === binding.key &&
        latest.current.visible.parentId === parentId
        ? parentId
        : null,
    );
    return next;
  };

  const accept = (client: RpcClient, slot: Slot, n: SideNotification): void => {
    // 帧自带的父 id 决定它归哪个控制器：一条连接上可能同时有多个父的旁支。
    const binding = findBinding(store, client, slot);
    if (!binding) return;
    const controller = controllerFor(binding, n.params.parent_thread_id);
    controller.accept(n);
  };

  useEffect(() => {
    const map = controllers.current;
    return () => {
      for (const controller of map.values()) controller.dispose();
      map.clear();
    };
  }, []);

  // 可见性同步：只有**当前显示的那一条**需要尾帧探测。
  //
  // 按完整身份匹配（hostKey + parentId），不是裸 parentId：两个宿主可以各有
  // 一条 `t1`，只看 parentId 会把两边的探测同时打开。后台宿主一律显式关掉。
  const visibleHostKey = visible === null ? null : visible.hostKey;
  const visibleParentId = visible === null ? null : visible.parentId;
  useEffect(() => {
    for (const controller of controllers.current.values()) {
      controller.setVisible(null);
    }
    if (visibleHostKey === null || visibleParentId === null) return;
    const controller = controllers.current.get(
      controllerKey(visibleHostKey, visibleParentId),
    );
    controller?.setVisible(visibleParentId);
  }, [visibleHostKey, visibleParentId]);

  return { accept, controllerFor };
}

/** 在该连接上找绑定。**不**回退到「当前连接」：旁支的归属是连接身份。 */
function findBinding(
  store: SideConversationStore,
  client: RpcClient,
  slot: Slot,
): SideHostBinding | null {
  for (const binding of store.bindingsForSlot(slot)) {
    if (binding.client === client) return binding;
  }
  return null;
}
/** 供 UI 读一条视图；不存在返回 undefined（不造壳）。 */
export function readSideView(
  store: SideConversationStore,
  hostKey: SideHostKey,
  sideId: string,
): SideView | undefined {
  return store.read(hostKey, sideId);
}

/** 重新导出，供调用方判别命名空间而不必再 import 两个模块。 */
export { isSideNotification };
