import { RpcClient, type RequestFn, type Transport } from "../lib/rpc";
import { LOCAL_SLOT, type Slot } from "../lib/slots";
import type { ClientRegistry } from "../lib/clientRegistry";
import { nextReconnectDelay } from "../lib/reconnect";
import {
  connectableTargets,
  parseRelayTargets,
  relayUrlForTarget,
} from "../lib/relayTargets";
import { storedRemoteConfig } from "../lib/remoteConfig";
import { wsTransport } from "../wsTransport";
import { formatError } from "../lib/errorMessage";
import {
  parseSideHandshake,
  type SideConversationStore,
} from "../lib/sideConversation";
import type { TraceConnection } from "../lib/subagentTraceSubscription";

/**
 * 连接生命周期（原 `App.tsx` 巨型 effect 内的建连/重连/前台唤醒，约 250 行）。
 *
 * 这里叫「工厂」而不是 hook，理由与 `useNotificationRouter` 完全相同：这些闭包
 * 必须看到**本次 effect 运行那一帧**的 `prefs` / `board` / `sess`。写成
 * `useCallback` 形状的 hook，依赖要么写不全（抓到旧值，静默错误），要么写全
 * （每次渲染换新身份，把下游 effect 拖进重跑）。故保留「调用方在 effect 里
 * 现建」的形式，依赖由参数显式传入，`tsc` 保证一个不漏。
 *
 * **本模块保留的唯一可变状态**是 `restartTimer` 与 `attempt`，它们由调用方
 * 持有并在多次 `reconnect*` 之间共享——语义是「跨重连保持」，放进工厂内部会
 * 每次重建都归零，退避就失效了。
 */
export interface ConnectionLifecycleDeps {
  activeSlotRef: React.MutableRefObject<Slot>;
  clientRef: React.MutableRefObject<RpcClient | null>;
  registryRef: React.MutableRefObject<ClientRegistry<RpcClient>>;
  /** 一次配对/配置变更对应一轮 effect；轮次内的退避定时器。 */
  restartTimer: { current: number | null };
  /** 连续失败次数。握手成功即清零，所以偶尔一次闪断不背更大的延迟。 */
  attempt: { current: number };
  disposed: () => boolean;
  toppedUp: React.MutableRefObject<Set<string>>;
  setStatus: (v: string) => void;
  transportFactory: (opts: { slot: Slot }) => Transport;
  /**
   * 中继自愈的裸请求钩子。
   *
   * 必须带上**发起它的那条连接**：服务端把连接判为未初始化时，重放 initialize
   * 是对那一条连接做的事。只给 `raw` 的话，重放期间的握手就绪通知无从归属，
   * 子 agent 轨迹会一直停在「未就绪」。
   */
  recoveryRef: React.MutableRefObject<
    ((raw: RequestFn, client: RpcClient, slot: Slot) => Promise<void>) | null
  >;
  /** 由 `createHandshakeRecovery` 提供：新客户端建好后要做的握手。 */
  handshake: (client: RpcClient) => Promise<void>;
  /** 由 `createNotificationRouter` 提供：给新客户端挂回调。 */
  wireClient: (client: RpcClient, slot: Slot) => void;
  /**
   * 登记一条新连接（槽位 + 服务端绑定代际 + 连接代际）。
   *
   * **必须先于 `wireClient`**：路由的回调闭包依赖「这条 client 属于哪个来源」，
   * 而来源只能从登记里查。反了就变成「先收帧、后知道帧该归谁」，早期帧会被
   * 当成未知来源丢掉——恰好是握手后第一段轨迹。
   */
  onTraceConnection: (connection: TraceConnection) => void;
  /** 本槽位当前的服务端绑定代际：换绑（重新配对）才递增，普通重连不变。 */
  serverBindingGeneration: (slot: Slot) => number;
  /** 下一条连接的代际号（单调递增，跨槽位共享）。 */
  nextTraceConnectionGeneration: () => number;
  /**
   * 旁支绑定登记处。
   *
   * 手机为每个远端项目各建一条连接，每条都要**独立**解析自己的能力位与宿主
   * id：不能拿本地那条答案去渲染远端项目，也不能因为本地支持就假定远端支持。
   */
  sideStore: SideConversationStore;
}

export interface ConnectionLifecycle {
  /** 建某槽位的连接。**只替换该槽位**，绝不动别的槽位。 */
  connectSlot: (slot?: Slot) => RpcClient;
  /** 让 `clientRef` 指向当前活动槽位的客户端。切槽位/重连后都要调一次。 */
  syncActiveClient: () => void;
  connect: () => RpcClient;
  /** 手机端：把 `remote/relayTargets` 里的每个可用项目各连一条。 */
  connectRemoteTargets: (overLocal: RpcClient) => Promise<void>;
  reconnectNow: () => void;
  reconnectSlot: (slot: Slot) => void;
  scheduleReconnect: () => void;
  /**
   * iOS 从后台切回前台时 socket 常已被系统静默掐断，状态不会自己回来。
   *
   * 前台唤醒一律重建客户端重连：延迟在这条路径上毫无价值。这里故意**不看**
   * `connected`——被后台掐断的 socket 可能没有 close/exited 事件，于是那个标志
   * 会陈旧地停在 true（半开连接），信它就会把恰恰要修的场景漏过去。
   *
   * 只在可见性事件上触发，正常会话不受影响；`reconnectNow` 先清在途定时器，
   * 再替换当前客户端，因此不会并发两条连接。
   *
   * 遍历**所有**槽位：被后台掐断的远端连接不会自己恢复。只重连当前槽位会让
   * 用户切回其它项目时的前几个请求全部失败。
   */
  onForegroundWake: () => void;
}

export function createConnectionLifecycle(
  deps: ConnectionLifecycleDeps,
): ConnectionLifecycle {
  const {
    activeSlotRef,
    clientRef,
    registryRef,
    restartTimer,
    attempt,
    disposed,
    toppedUp,
    setStatus,
    transportFactory,
    recoveryRef,
    handshake,
    wireClient,
    sideStore,
    onTraceConnection,
    serverBindingGeneration,
    nextTraceConnectionGeneration,
  } = deps;

  /**
   * 建某槽位的连接。**只替换该槽位**，绝不动别的槽位。
   *
   * 销毁旧实例的职责收在 `ClientRegistry.put`：Tauri 事件监听不会随连接关闭
   * 自动停，未 dispose 的旧 transport 会与新的一起消费每条帧，append-only 的
   * `item/delta` 于是被应用两遍（流式文本重复，直到 `item/completed` 按 id
   * 整条覆盖才「收敛」）。
   */
  const connectSlot = (slot: Slot = LOCAL_SLOT): RpcClient => {
    // 新连接 = 新的对账周期：为「none 缓存会话」的静默补齐重新开账（见 `toppedUp`）。
    toppedUp.current.clear();
    const client: RpcClient = new RpcClient(transportFactory({ slot }), (raw) =>
      recoveryRef.current!(raw, client, slot),
    );
    // 登记先于 wireClient：路由拿到帧时按 client 反查来源，晚一步就会丢帧。
    onTraceConnection({
      slot,
      serverBindingGeneration: serverBindingGeneration(slot),
      connectionGeneration: nextTraceConnectionGeneration(),
      client,
    });
    registryRef.current.put(slot, client);
    wireClient(client, slot);
    return client;
  };

  /** 让 `clientRef` 指向当前活动槽位的客户端。切槽位/重连后都要调一次。 */
  const syncActiveClient = () => {
    registryRef.current.setActive(activeSlotRef.current);
    clientRef.current = registryRef.current.current() ?? null;
  };

  const connect = (): RpcClient => {
    const client = connectSlot(activeSlotRef.current);
    syncActiveClient();
    return client;
  };

  /**
   * 手机端：把 `remote/relayTargets` 里的每个可用项目各连一条。
   *
   * 三条约束：
   * - 只连 `available` 且带 `session` 的项。不可用 = 那台远端的进程还没握手；
   *   `session` 为 null = 本设备是 `Observe` 域，凭据根本没发给它。
   * - **单个失败不影响其它**：多连接的全部价值就在于此。某个 target 建连失败
   *   只是它自己缺席，不抛、不回退、不删已建好的连接。
   * - 已经在册的槽位**不重建**：握手里会跑多次（重连、切回前台），每次重建
   *   都会掐掉一条正在用的连接，用户看到的是流式输出中断。
   */
  const connectRemoteTargets = async (overLocal: RpcClient) => {
    let raw: unknown;
    try {
      raw = await overLocal.request("remote/relayTargets", {});
    } catch {
      // 旧宿主 / stable 上这个方法不存在（-32601）。手机不该因为对面版本老
      // 就连本地会话都用不了，静默当「没有远端项目」。
      return;
    }
    const targets = parseRelayTargets(raw);
    const config = storedRemoteConfig(localStorage);
    for (const target of connectableTargets(targets)) {
      if (disposed()) return;
      if (registryRef.current.get(target.profileId)) continue;
      try {
        const url = config
          ? relayUrlForTarget(config.url, target.session)
          : target.session;
        if (!config) continue;
        const next: RpcClient = new RpcClient(wsTransport(url, config.token), (r) =>
          recoveryRef.current!(r, next, target.profileId),
        );
        onTraceConnection({
          slot: target.profileId,
          serverBindingGeneration: serverBindingGeneration(target.profileId),
          connectionGeneration: nextTraceConnectionGeneration(),
          client: next,
        });
        registryRef.current.put(target.profileId, next);
        wireClient(next, target.profileId);
        // 与主连接同样握手：不做 `initialize` 的话该连接上任何 `thread/*` 都
        // 会得到 `-32010 not initialized`。
        const initialized = await next.request<Record<string, unknown>>(
          "initialize",
          {},
        );
        // 旁路连接也要解析旁支能力与宿主 id（spec §8.1「包括旁路连接握手」）。
        // 漏了这一条，手机上从「项目列表」切进某个远端项目时旁支入口永远不出现
        // ——而那条连接其实完全支持。
        const scope = storedRemoteConfig(localStorage)?.scope ?? "control";
        const binding = parseSideHandshake(
          target.profileId,
          next,
          initialized,
          scope,
        );
        if (binding) sideStore.bind(binding);
      } catch (e) {
        // 只记这个项目：把它从注册表摘掉，避免留一条半死连接反复报错。
        registryRef.current.remove(target.profileId);
        console.warn(`remote project ${target.profileId} unavailable:`, e);
      }
    }
  };

  // 断开后的重连入口：清掉可能已在途的定时器，立刻建客户端重试。前台唤醒
  // 用它做即时重连，exited 处理则用 scheduleReconnect 排队。
  const reconnectNow = () => {
    if (disposed()) return;
    if (restartTimer.current !== null) {
      window.clearTimeout(restartTimer.current);
      restartTimer.current = null;
    }
    let next: RpcClient;
    try {
      next = connect();
    } catch (e) {
      // 建 transport 本身也可能同步抛（落盘 URL 格式坏掉时 `withToken` 里的
      // `new URL` 就会）：这同样是「连不上」，必须继续退避，否则循环就此死掉。
      setStatus(`error: ${formatError(e)}`);
      scheduleReconnect();
      return;
    }
    void handshake(next);
  };

  /**
   * 重建**某个**槽位的连接并握手。
   *
   * 与 `reconnectNow` 的区别：那个只服务「当前活动槽位 + 单一退避定时器」，
   * 这条要给「切回前台时把所有槽位一起唤醒」用——远端槽位被系统掐断后不会
   * 自己恢复，逐个重建才不会有项目静默半死。
   *
   * **只握手、不排队**：这里的调用点是前台唤醒，延迟没有价值；失败了由
   * 该槽位自己的握手失败路径（`scheduleReconnect`）接管。
   */
  const reconnectSlot = (slot: Slot) => {
    if (disposed()) return;
    let next: RpcClient;
    try {
      next = connectSlot(slot);
    } catch (e) {
      setStatus(`error: ${formatError(e)}`);
      return;
    }
    syncActiveClient();
    void handshake(next);
  };

  // 退避排队：只要组件还挂着就绝不停下重试，后端没起来也不会一直卡在 connecting。
  const scheduleReconnect = () => {
    if (disposed() || restartTimer.current !== null) return;
    restartTimer.current = window.setTimeout(() => {
      restartTimer.current = null;
      reconnectNow();
    }, nextReconnectDelay(attempt.current));
    attempt.current += 1;
  };

  const onForegroundWake = () => {
    if (document.visibilityState !== "visible" || disposed()) return;
    attempt.current = 0;
    const slots = registryRef.current.keys();
    if (slots.length <= 1) {
      reconnectNow();
      return;
    }
    for (const slot of slots) reconnectSlot(slot);
  };

  return {
    connectSlot,
    syncActiveClient,
    connect,
    connectRemoteTargets,
    reconnectNow,
    reconnectSlot,
    scheduleReconnect,
    onForegroundWake,
  };
}
