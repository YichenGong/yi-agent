/**
 * `@yi-agent/plugin-sdk`
 *
 * 插件视图（iframe 里的页面）用它跟宿主对话。这个包**故意很小**：
 *
 * - 它不含宿主的管理 RPC 客户端，也不 import Tauri。页面没有别的通路能拿到
 *   宿主能力，SDK 也就不是"另一条后门"。
 * - 它只做三件事：解析宿主发来的握手、把消息按线协议发出去、把事件分发回来。
 * - 身份（插件 / 项目 / 代次）不在报文里，由宿主按 frame 绑定。SDK 因此
 *   没有任何"我是谁"的参数可传——少一个可伪造的入口。
 *
 * 线协议类型是这里的**自有一份**，与宿主侧的同名类型对齐由契约检查脚本的
 * golden 对照保证（T18-j）。之所以不复用宿主源码：那会让构建产物里出现宿主
 * 内部路径，插件包就绑死在某个桌面版本上。产物必须能被单独 `npm run build`，
 * 且不含任何宿主模块名。
 */

/** 本 SDK 实现的界面 API 版本。宿主只接受它认识的主版本。 */
export const BRIDGE_API_VERSION = "v1" as const;

/** 宿主注入的全局名。页面不需要知道它——`connect()` 会自己找。 */
export const HOST_GLOBAL = "__yiAgentPluginHost" as const;

/** 本 SDK 认识的 `OperationResult` 判别式；由契约 golden 对照固定。 */
export const RESPONSE_VARIANTS = ["pending", "committed", "unknown", "rejected"] as const;

/** 圈定桥的能力边界的那份前缀；管理方法不走桥。 */
export const MANAGEMENT_PREFIX = "plugins/v1/" as const;

/** 稳定的错误分类，与宿主 `ErrorKind` 一一对应。 */
export type ErrorKind =
  | "InvalidManifest"
  | "Incompatible"
  | "Untrusted"
  | "NotFound"
  | "Conflict"
  | "PermissionDenied"
  | "NotSupported"
  | "Draining"
  | "Busy"
  | "ResultUnknown"
  | "RecoveryRequired"
  | "Integrity"
  | "QuotaExceeded"
  | "RateLimited"
  | "TooLarge"
  | "Io";

export interface BridgeError {
  kind: ErrorKind;
  detail: string;
}

export type OperationStatus = "pending" | "committed" | "unknown" | "rejected";

export interface OperationResult<T = unknown> {
  status: OperationStatus;
  requestId: string;
  result: T | null;
}

export interface PluginContext {
  api: string;
  plugin: { id: string; version: string };
  host: { id: string; version: string };
  project: { handle: string };
  activation: number;
  theme: "light" | "dark";
}

export interface BridgeEvent<T = unknown> {
  topic: string;
  revision: number;
  payload: T;
}

export interface Snapshot<T = unknown> {
  topic: string;
  revision: number;
  state: T;
}

export type NotifyLevel = "info" | "warning" | "error";

export interface ConfirmRequest {
  title: string;
  body: string;
  confirmLabel?: string;
  cancelLabel?: string;
  destructive?: boolean;
}

/** 页面能看到的全部宿主能力。名字与协议 spec 冻结的 7 个方法一致。 */
export interface PluginHostApi {
  context(): Promise<PluginContext>;
  request<T = unknown>(method: string, params: unknown): Promise<T>;
  result<T = unknown>(requestId: string): Promise<OperationResult<T>>;
  subscribe<T = unknown>(
    topic: string,
    onEvent: (event: BridgeEvent<T>) => void,
  ): Promise<() => void>;
  openThread(threadId: string): Promise<void>;
  notify(level: NotifyLevel, message: string): Promise<void>;
  confirm(request: ConfirmRequest): Promise<boolean>;
}

/** `connect()` 失败时抛出，携带稳定的 kind，便于页面分支处理。 */
export class PluginBridgeError extends Error {
  constructor(
    readonly kind: ErrorKind,
    readonly detail: string,
  ) {
    super(`${kind}: ${detail}`);
    this.name = "PluginBridgeError";
  }
}

/**
 * 与 `MessagePort` / `Window` 都兼容的最小端口形状。
 *
 * 用结构类型而不是 `MessagePort`：SDK 不该假设页面跑在 iframe 里，也不该
 * 依赖 DOM 类型定义。
 */
export interface MessagePortLike {
  postMessage(message: unknown): void;
  addEventListener(type: "message", listener: (event: { data: unknown }) => void): void;
  removeEventListener?(type: "message", listener: (event: { data: unknown }) => void): void;
  start?(): void;
}

interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (error: PluginBridgeError) => void;
}

interface Subscription {
  topic: string;
  handler: (event: BridgeEvent) => void;
}

/** 宿主发给页面的握手消息。 */
export interface HostHello {
  type: "hello";
  api: string;
  /** 页面把自己的 `init` 回复到这个端口；默认就是握手来的端口。 */
  context: PluginContext;
  token: string;
}

/** 页面回给宿主的握手确认。 */
export interface HostInit {
  type: "init";
  nonce: string;
  uiApi: string;
}

export interface ConnectOptions {
  /** 显式指定端口；缺省时从 `globalThis[HOST_GLOBAL]` 取。 */
  port?: MessagePortLike;
  /** 页面自己生成的 nonce；缺省时用 crypto。 */
  nonce?: string;
  /** 等待宿主握手的最长时间。 */
  helloTimeoutMs?: number;
}

const DEFAULT_HELLO_TIMEOUT = 10_000;

/**
 * 连接宿主。返回的 API 里所有方法都在同一个端口上收发消息。
 *
 * 失败一律抛 `PluginBridgeError`，不返回"半可用"对象：调用方要么拿到完整
 * 能力，要么明确知道连不上。
 */
export async function connect(options: ConnectOptions = {}): Promise<PluginHostApi> {
  const port = options.port ?? resolveInjectedPort();
  const nonce = options.nonce ?? randomNonce();

  let sequence = 0;
  const nextId = (): string => `sdk-${Date.now().toString(36)}-${(sequence += 1).toString(36)}`;

  const pending = new Map<string, PendingRequest>();
  const subscriptions = new Map<string, Subscription>();
  let token: string | null = null;
  let helloResolve!: (hello: HostHello) => void;
  let helloReject!: (error: PluginBridgeError) => void;
  const hello = new Promise<HostHello>((resolve, reject) => {
    helloResolve = resolve;
    helloReject = reject;
  });

  const send = (message: unknown): void => {
    port.postMessage(message);
  };

  const listener = (event: { data: unknown }): void => {
    const data = event.data;
    if (!isRecord(data)) return;
    switch (data.type) {
      case "hello": {
        token = String(data.token ?? "");
        helloResolve({
          type: "hello",
          api: String(data.api ?? ""),
          context: data.context as PluginContext,
          token,
        });
        return;
      }
      case "response": {
        const requestId = String(data.requestId ?? "");
        const settled = pending.get(requestId);
        if (!settled) return;
        pending.delete(requestId);
        if (data.ok === true) {
          settled.resolve(data.result);
        } else {
          const error = data.error;
          settled.reject(
            isRecord(error)
              ? new PluginBridgeError(error.kind as ErrorKind, String(error.detail ?? ""))
              : new PluginBridgeError("Io", "host returned an unreadable error"),
          );
        }
        return;
      }
      case "event": {
        const subscription = subscriptions.get(String(data.topic ?? ""));
        if (!subscription) return;
        subscription.handler({
          topic: subscription.topic,
          revision: Number(data.revision ?? 0),
          payload: data.payload,
        });
        return;
      }
      default:
        return;
    }
  };

  port.addEventListener("message", listener);
  port.start?.();

  const timeout = options.helloTimeoutMs ?? DEFAULT_HELLO_TIMEOUT;
  const timer = setTimeout(() => {
    helloReject(new PluginBridgeError("Io", "the host did not complete the handshake in time"));
  }, timeout);

  try {
    send({ type: "init", nonce, uiApi: BRIDGE_API_VERSION } satisfies HostInit);
    const greeting = await hello;
    if (greeting.api !== BRIDGE_API_VERSION) {
      throw new PluginBridgeError(
        "Incompatible",
        `the host speaks "${greeting.api}", this SDK implements "${BRIDGE_API_VERSION}"`,
      );
    }
  } finally {
    clearTimeout(timer);
  }

  const call = <T>(method: string, params: unknown): Promise<T> => {
    if (token === null) {
      return Promise.reject(new PluginBridgeError("PermissionDenied", "the bridge is not connected"));
    }
    const requestId = nextId();
    return new Promise<T>((resolve, reject) => {
      pending.set(requestId, {
        resolve: (value) => resolve(value as T),
        reject,
      });
      send({ type: "request", requestId, method, params, token });
    });
  };

  return {
    async context() {
      const greeting = await hello;
      return greeting.context;
    },
    request<T = unknown>(method: string, params: unknown) {
      return call<T>(method, params);
    },
    async result<T = unknown>(requestId: string) {
      return call<OperationResult<T>>("plugins/v1/operationResult", { requestId });
    },
    async subscribe<T = unknown>(topic: string, onEvent: (event: BridgeEvent<T>) => void) {
      const previous = subscriptions.get(topic);
      if (previous) {
        throw new PluginBridgeError("Conflict", `already subscribed to "${topic}"`);
      }
      const entry: Subscription = {
        topic,
        handler: onEvent as (event: BridgeEvent) => void,
      };
      subscriptions.set(topic, entry);
      try {
        const snapshot = await call<Snapshot | null>("subscribe", { topic });
        if (snapshot && typeof snapshot.revision === "number") {
          // SDK 只负责把快照交给回调，宿主已经按快照修订号续订过了。
          onEvent({
            topic,
            revision: snapshot.revision,
            payload: snapshot.state,
          } as BridgeEvent<T>);
        }
      } catch (error) {
        subscriptions.delete(topic);
        throw error;
      }
      return () => {
        subscriptions.delete(topic);
        send({ type: "unsubscribe", topic, token });
      };
    },
    async openThread(threadId: string) {
      await call("openThread", { threadId });
    },
    async notify(level: NotifyLevel, message: string) {
      await call("notify", { level, message });
    },
    async confirm(request: ConfirmRequest) {
      return call<boolean>("confirm", request);
    },
  };
}

/**
 * 页面入口常用的便捷包装：拿不到宿主时返回 `null`，而不是抛错。
 * 用法上只有"要不要渲染业务 UI"这一处需要判断。
 */
export async function connectOrNull(options: ConnectOptions = {}): Promise<PluginHostApi | null> {
  try {
    return await connect(options);
  } catch (error) {
    if (error instanceof PluginBridgeError) return null;
    throw error;
  }
}

function resolveInjectedPort(): MessagePortLike {
  const injected = (globalThis as Record<string, unknown>)[HOST_GLOBAL];
  if (isPort(injected)) return injected;
  throw new PluginBridgeError(
    "PermissionDenied",
    `this page has no host bridge: "${HOST_GLOBAL}" is not injected`,
  );
}

function isPort(value: unknown): value is MessagePortLike {
  return (
    isRecord(value) &&
    typeof value.postMessage === "function" &&
    typeof value.addEventListener === "function"
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function randomNonce(): string {
  const bytes = new Uint8Array(16);
  globalThis.crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}
