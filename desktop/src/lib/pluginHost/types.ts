/**
 * 插件宿主桥的线协议类型（v1）。
 *
 * 这里是**唯一**定义桥报文形状的地方：宿主（本文件所在的 desktop）与 SDK
 * （`plugins/sdk`）都从这里/它的构建产物取类型。两边各写一份结构，迟早会
 * 有一边多一个可选字段，而"多出来的那个字段"正是越权的入口。
 *
 * 三条不变量（设计 §8、协议 spec §7、§9.3）：
 *
 * 1. **身份由宿主判定，不由报文声明。** 报文里没有 `plugin`、`project`、
 *    `activation` 这类字段。它们全部来自宿主在 `init` 时绑定的 frame。
 * 2. **写操作按 `(requestId, method, params)` 幂等。** 同 ID 同参数返回原
 *    结果；同 ID 改参数是 `Conflict`；结果未知时是 `unknown`，客户端不得
 *    自行重试。
 * 3. **事件带单调修订号。** 收到不连续的 `revision` 时必须先取快照，
 *    不能靠"丢了几个事件"去推状态。
 */

/** 稳定的错误分类。与协议 spec §9.1 的 16 个 kind 对齐（只用这里列出的）。 */
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

/** 桥层能产生的错误。`detail` 面向日志与提示，不承载授权信息。 */
export interface BridgeError {
  kind: ErrorKind;
  detail: string;
}

/**
 * 宿主为一次对话绑定的身份。
 *
 * `activation` 是**当前已提交的代次**；`grant` 是项目授权的代次。两者都可以
 * 在会话进行中递增，递增之后旧通道立即失效（协议 spec §7.3）。
 */
export interface HostBinding {
  /** 宿主实例 id（本机 / 远端各一个）。 */
  host: string;
  /** 插件 id，来自宿主对 frame 的绑定，不来自报文。 */
  plugin: string;
  /** 项目句柄，形如 `ph1_...`。 */
  project: string;
  /** 已提交的激活代次。 */
  activation: number;
  /** 项目授权的代次。 */
  grant: number;
  /** 界面 API 版本（例如 `v1`）。 */
  uiApi: string;
}

/** SDK 侧 `context()` 的返回值：只读快照。 */
export interface PluginContext {
  api: string;
  plugin: { id: string; version: string };
  host: { id: string; version: string };
  project: { handle: string };
  activation: number;
  theme: "light" | "dark";
}

/** `OperationResult` 的判别式。与 `bridge-v1.schema.json` 的 `responseVariants` 一致。 */
export type OperationStatus = "pending" | "committed" | "unknown" | "rejected";

export interface OperationResult<T = unknown> {
  status: OperationStatus;
  requestId: string;
  result: T | null;
}

/** 一次 `request()` 的响应。 */
export type BridgeResponse<T = unknown> =
  | { ok: true; requestId: string; result: T }
  | { ok: false; requestId: string; error: BridgeError };

/** 服务端推送的事件。`revision` 单调递增，缺口意味着必须取快照。 */
export interface BridgeEvent<T = unknown> {
  topic: string;
  revision: number;
  payload: T;
}

/** 快照响应：先快照，再按其 `revision` 续订（协议 spec §7.2）。 */
export interface Snapshot<T = unknown> {
  topic: string;
  revision: number;
  state: T;
}

/**
 * 页面 → 宿主的唯一入口。SDK 只负责"把消息送到这个对象"，
 * 它自己不知道宿主是 Tauri 还是别的东西。
 */
export interface BridgeTransport {
  /** 发送一条消息，解析为响应。实现方决定底层是 MessagePort 还是 postMessage。 */
  send(message: BridgeRequest): Promise<BridgeResponse>;
}

export interface BridgeRequest {
  requestId: string;
  method: string;
  params: unknown;
  /**
   * 宿主在 `init` 时发给**这一个 frame** 的不透明令牌。
   *
   * 报文里没有插件/项目/代次字段，就是因为它们不能由报文声明：令牌只证明
   * "这条消息来自那个被宿主编排过的 frame"，其余身份全部由宿主侧的绑定决定。
   * 换了 frame、换了代次，令牌就换，旧令牌的消息一律 `PermissionDenied`。
   */
  token?: string;
}

/** 握手：每个 frame 恰好一次。`nonce` 由页面生成，宿主只用于防重放。 */
export interface InitializationRequest {
  type: "init";
  nonce: string;
  /** 页面声明的界面 API 版本；宿主只接受自己支持的版本。 */
  uiApi: string;
}

export interface InitializationResult {
  token: string;
  context: PluginContext;
  /** 一个 frame 只能握手一次；重连要新建 frame。 */
  readonly once: true;
}

/** `notify()` 允许的等级。宿主可以据此决定是否要打扰用户。 */
export type NotifyLevel = "info" | "warning" | "error";

/** `confirm()` 的请求体。宿主负责渲染，插件不画自己的确认框。 */
export interface ConfirmRequest {
  title: string;
  body: string;
  confirmLabel?: string;
  cancelLabel?: string;
  destructive?: boolean;
}

/**
 * 桥的 7 个方法（协议 spec §13 冻结了名字）。
 *
 * `result` 与 `operationResult` 是同一个东西的两端：`result(requestId)` 查
 * 一个已经发出、但结果未知的写操作。名称在 SDK 里是 `result`，在管理 RPC 里
 * 是 `plugins/v1/operationResult`。
 */
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

// ---------------------------------------------------------------------------
// 线协议的常量与校验（宿主与 SDK 共用同一份判定）
// ---------------------------------------------------------------------------

/**
 * 桥允许的方法：只有业务方法。管理方法的 `plugins/v1/*` 前缀**不**经桥，
 * 否则插件能给自己装/授权（协议 spec §11 的闭合理由）。
 */
export const MANAGEMENT_PREFIX = "plugins/v1/";

/**
 * `OperationResult` 的**全部**判别式，顺序与 `bridge-v1.schema.json` 的
 * `responseVariants` 一致。它存在的原因是双端 golden 对照（T18-j）：
 * checker 逐条比对宿主与 SDK 的这份列表，少一个或自己发明一个都会红。
 */
export const RESPONSE_VARIANTS = ["pending", "committed", "unknown", "rejected"] as const;

export const DEFAULT_LIMITS = {
  /** 单条消息的字节上限。 */
  maxMessageBytes: 256 * 1024,
  /** 同时在途的请求数上限。 */
  maxInFlight: 32,
  /** 每窗口每秒的请求上限（令牌桶容量）。 */
  maxRequestsPerSecond: 60,
  /** 每窗口的订阅上限。 */
  maxSubscriptions: 16,
} as const;
