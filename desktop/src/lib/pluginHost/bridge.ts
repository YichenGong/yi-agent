/**
 * 宿主侧桥。**所有身份判定都在这里完成，且只看 frame。**
 *
 * 报文里没有 plugin / project / activation，因此"插件自报身份"这条路在结构上
 * 就不存在（T18-a）。每个 frame 在 `init` 时拿到一个随机令牌，此后每条消息都
 * 必须带它；令牌只证明"这条消息来自那个被排序过的 frame"，不携带任何可伪造
 * 的权限信息。
 *
 * 写操作的语义（协议 spec §9.3）：
 * - `(requestId, method, params)` 幂等；同 ID 改参数 → `Conflict`。
 * - 已送出但未确认 → `unknown`，**不自动重发**（T18-d）。
 * - `grant` / `activation` 递增后旧响应一律丢弃，不投递给新页面（T18-c）。
 */

import { EventHub, type SubscribeHandle } from "./events";
import {
  DEFAULT_LIMITS,
  MANAGEMENT_PREFIX,
  type BridgeError,
  type BridgeRequest,
  type BridgeResponse,
  type ConfirmRequest,
  type ErrorKind,
  type HostBinding,
  type InitializationRequest,
  type InitializationResult,
  type NotifyLevel,
  type OperationResult,
  type OperationStatus,
  type PluginContext,
  type Snapshot,
} from "./types";

/** 后端抛出的可分类失败。桥把它原样翻译成 `BridgeError`。 */
export class BridgeFailure extends Error {
  constructor(
    readonly kind: ErrorKind,
    readonly detail: string,
  ) {
    super(`${kind}: ${detail}`);
    this.name = "BridgeFailure";
  }
}

export interface BackendRequest {
  method: string;
  params: unknown;
}

export interface BridgeBackend {
  /** 把一次业务调用转给插件自己的后端。抛错表示失败。 */
  forward(binding: HostBinding, request: BackendRequest): Promise<{ result: unknown }>;
  /** 取 topic 的权威快照；缺省时事件缺口不可恢复。 */
  snapshot?(binding: HostBinding, topic: string): Promise<Snapshot>;
}

export interface BridgePresent {
  notify(level: NotifyLevel, message: string): Promise<void>;
  confirm(request: ConfirmRequest): Promise<boolean>;
  openThread(threadId: string): Promise<void>;
}

/** 一个 frame 的静态信息。frame 本身是宿主创建的真实对象（iframe contentWindow）。 */
export interface FrameRegistration {
  binding: HostBinding;
  /** 插件声明的业务方法名。不在表内一律 `NotSupported`。 */
  methods: readonly string[];
  /** `openThread` 允许的线程 id 集合；缺省表示该项目下的任意线程。 */
  threads?: readonly string[];
  plugin: { id: string; version: string };
  host: { id: string; version: string };
}

export interface BridgeLimits {
  maxMessageBytes: number;
  maxInFlight: number;
  maxRequestsPerSecond: number;
  maxSubscriptions: number;
}

export interface PluginBridgeOptions {
  backend: BridgeBackend;
  present: BridgePresent;
  limits?: Partial<BridgeLimits>;
  /** 当前主题；页面重绘时不需要重新握手。 */
  theme?: () => "light" | "dark";
  now?: () => number;
  /** 令牌与 nonce 生成；测试可注入确定性实现。 */
  randomToken?: () => string;
}

interface FrameState {
  registration: FrameRegistration;
  token: string | null;
  nonce: string | null;
  inFlight: number;
  windowStartedAt: number;
  windowCount: number;
  subscriptions: Set<SubscribeHandle>;
  /** 每当代次变化递增；旧代次的响应按此丢弃。 */
  epoch: number;
}

interface LedgerEntry {
  key: string;
  status: OperationStatus;
  result: unknown;
  error: BridgeError | null;
  /** 发起该请求的绑定额度（host/plugin/project/activation）。代次变化时只作废自己范围内的条目。 */
  scope: string;
}

function scopeOf(binding: HostBinding): string {
  return [binding.host, binding.plugin, binding.project, binding.activation].join("|");
}

const encoder = new TextEncoder();

export class PluginBridge {
  private readonly frames = new WeakMap<object, FrameState>();
  private readonly ledger = new Map<string, LedgerEntry>();
  private readonly usedNonces = new Set<string>();
  private readonly hub: EventHub;
  /** topic → 发起订阅的 frame 绑定，快照必须用同一个绑定取。 */
  private readonly topicBindings = new Map<string, HostBinding>();
  private readonly limits: BridgeLimits;
  private readonly now: () => number;
  private readonly randomToken: () => string;

  constructor(private readonly options: PluginBridgeOptions) {
    this.limits = { ...DEFAULT_LIMITS, ...options.limits };
    this.now = options.now ?? (() => Date.now());
    this.randomToken = options.randomToken ?? defaultToken;
    this.hub = new EventHub({
      snapshot: async (topic) => this.snapshotFor(topic),
    });
  }

  // -------------------------------------------------------------------------
  // frame 生命周期
  // -------------------------------------------------------------------------

  /**
   * 登记一个宿主创建的 frame。重复登记视为编排错误，直接抛。
   * 这里不接受任何"页面说自己是谁"的输入。
   */
  registerFrame(frame: object, registration: FrameRegistration): void {
    if (this.frames.has(frame)) {
      throw new Error("frame is already registered — one frame, one binding");
    }
    this.frames.set(frame, {
      registration,
      token: null,
      nonce: null,
      inFlight: 0,
      windowStartedAt: this.now(),
      windowCount: 0,
      subscriptions: new Set(),
      epoch: 0,
    });
  }

  /**
   * 握手。每个 frame 恰好一次；重复握手与 nonce 复用都拒绝（T18-b）。
   */
  initialize(frame: object, request: InitializationRequest): InitializationResult {
    const state = this.requireFrame(frame);
    if (state.token !== null) {
      throw new BridgeFailure("PermissionDenied", "this frame has already been initialized");
    }
    if (typeof request.nonce !== "string" || request.nonce.length === 0) {
      throw new BridgeFailure("PermissionDenied", "init requires a nonce");
    }
    if (this.usedNonces.has(request.nonce)) {
      throw new BridgeFailure("PermissionDenied", "nonce has already been used");
    }
    if (request.uiApi !== "v1") {
      throw new BridgeFailure(
        "Incompatible",
        `this host speaks ui api "v1", the page asked for "${request.uiApi}"`,
      );
    }
    this.usedNonces.add(request.nonce);
    state.token = this.randomToken();
    state.nonce = request.nonce;
    return {
      token: state.token,
      context: this.contextFor(state.registration),
      once: true,
    };
  }

  /**
   * 页面卸载 / frame 被销毁：清除订阅、把在途写标成 unknown。
   * 返回移除的订阅数（T18-h 用计数断言）。
   */
  dropFrame(frame: object): number {
    const state = this.frames.get(frame);
    if (!state) return 0;
    const removed = state.subscriptions.size;
    for (const handle of state.subscriptions) handle.unsubscribe();
    state.subscriptions.clear();
    const scope = scopeOf(state.registration.binding);
    for (const entry of this.ledger.values()) {
      if (entry.status === "pending" && entry.scope === scope) entry.status = "unknown";
    }
    state.token = null;
    state.inFlight = 0;
    return removed;
  }

  /**
   * 代次变化（grant 递增、激活切换、项目切换）：旧通道立即失效。
   * 旧代次仍在途的响应会被丢弃（T18-c）。
   */
  rebind(frame: object, binding: HostBinding): number {
    const state = this.requireFrame(frame);
    const previous = scopeOf(state.registration.binding);
    state.registration = { ...state.registration, binding };
    state.epoch += 1;
    const removed = state.subscriptions.size;
    for (const handle of state.subscriptions) handle.unsubscribe();
    state.subscriptions.clear();
    for (const entry of this.ledger.values()) {
      if (entry.status === "pending" && entry.scope === previous) entry.status = "unknown";
    }
    return removed;
  }

  /** 移除全部 frame 的订阅（宿主切换）。 */
  clearAllSubscriptions(): number {
    this.topicBindings.clear();
    return this.hub.clear();
  }

  // -------------------------------------------------------------------------
  // 请求处理
  // -------------------------------------------------------------------------

  async receive(frame: object, message: BridgeRequest): Promise<BridgeResponse> {
    const state = this.frames.get(frame);
    if (!state) {
      return fail(String(message?.requestId ?? ""), "PermissionDenied", "frame is not registered with this host");
    }
    const requestId = typeof message?.requestId === "string" ? message.requestId : "";
    if (requestId.length === 0) {
      return fail("", "NotSupported", "requestId is required");
    }
    if (state.token === null) {
      return fail(requestId, "PermissionDenied", "frame has not completed the init handshake");
    }
    if (message.token !== state.token) {
      return fail(
        requestId,
        "PermissionDenied",
        "this message did not come from the frame it is being delivered to",
      );
    }

    const size = encoder.encode(JSON.stringify(message)).byteLength;
    if (size > this.limits.maxMessageBytes) {
      return fail(requestId, "TooLarge", `message is ${size} bytes, limit is ${this.limits.maxMessageBytes}`);
    }

    const method = message.method;
    if (typeof method !== "string" || method.length === 0) {
      return fail(requestId, "NotSupported", "method is required");
    }
    if (method.startsWith(MANAGEMENT_PREFIX)) {
      // 管理方法是宿主的权限，不是插件的。桥不提供这条通路。
      return fail(
        requestId,
        "PermissionDenied",
        `bridge never carries management methods ("${method}"); they are host-only`,
      );
    }
    if (!state.registration.methods.includes(method)) {
      return fail(requestId, "NotSupported", `plugin does not declare a method named "${method}"`);
    }

    const key = `${method}\u0000${stableStringify(message.params ?? null)}`;
    const prior = this.ledger.get(requestId);
    if (prior) {
      if (prior.key !== key) {
        return fail(
          requestId,
          "Conflict",
          `requestId "${requestId}" was already used with different method or params — refusing to overwrite`,
        );
      }
      return {
        ok: prior.status === "committed",
        requestId,
        ...(prior.status === "committed"
          ? { result: prior.result }
          : {
              error:
                prior.error ??
                ({
                  kind: prior.status === "rejected" ? "Io" : "ResultUnknown",
                  detail: `request settled as ${prior.status}`,
                } satisfies BridgeError),
            }),
      } as BridgeResponse;
    }

    if (state.inFlight >= this.limits.maxInFlight) {
      return fail(
        requestId,
        "Busy",
        `${state.inFlight} requests are already in flight, limit is ${this.limits.maxInFlight}`,
      );
    }
    if (!this.takeToken(state)) {
      return fail(
        requestId,
        "RateLimited",
        `more than ${this.limits.maxRequestsPerSecond} requests in one second`,
      );
    }

    const epoch = state.epoch;
    const binding = state.registration.binding;
    const scope = scopeOf(binding);
    this.ledger.set(requestId, { key, status: "pending", result: null, error: null, scope });
    state.inFlight += 1;
    try {
      const outcome = await this.options.backend.forward(binding, {
        method,
        params: message.params ?? null,
      });
      if (state.epoch !== epoch) {
        // 代次已变：旧响应不属于新页面，丢弃内容并标记 unknown（T18-c）。
        this.ledger.set(requestId, { key, status: "unknown", result: null, error: null, scope });
        return fail(
          requestId,
          "ResultUnknown",
          "the binding changed while this request was in flight; the result was discarded",
        );
      }
      this.ledger.set(requestId, { key, status: "committed", result: outcome.result, error: null, scope });
      return { ok: true, requestId, result: outcome.result };
    } catch (error) {
      const bridgeError = toBridgeError(error);
      if (state.epoch !== epoch) {
        this.ledger.set(requestId, { key, status: "unknown", result: null, error: null, scope });
        return fail(requestId, "ResultUnknown", "the binding changed while this request was in flight");
      }
      this.ledger.set(requestId, { key, status: "rejected", result: null, error: bridgeError, scope });
      return { ok: false, requestId, error: bridgeError };
    } finally {
      state.inFlight -= 1;
    }
  }

  /** 按 requestId 查询已提交结果；未知即 `unknown`，绝不猜成失败（T18-d）。 */
  result(frame: object, requestId: string): OperationResult {
    this.requireFrame(frame);
    const entry = this.ledger.get(requestId);
    if (!entry) {
      return { status: "unknown", requestId, result: null };
    }
    return { status: entry.status, requestId, result: entry.result };
  }

  // -------------------------------------------------------------------------
  // 订阅
  // -------------------------------------------------------------------------

  async subscribe<T>(
    frame: object,
    topic: string,
    after: number,
    handler: (event: { topic: string; revision: number; payload: T }) => void,
  ): Promise<SubscribeHandle<T>> {
    const state = this.requireFrame(frame);
    if (state.subscriptions.size >= this.limits.maxSubscriptions) {
      throw new BridgeFailure(
        "Busy",
        `${state.subscriptions.size} subscriptions are already open, limit is ${this.limits.maxSubscriptions}`,
      );
    }
    this.topicBindings.set(topic, state.registration.binding);
    const handle = await this.hub.subscribe<T>(topic, after, handler);
    const tracked: SubscribeHandle = {
      revision: handle.revision,
      snapshot: handle.snapshot,
      unsubscribe: () => {
        handle.unsubscribe();
        state.subscriptions.delete(tracked);
      },
    };
    state.subscriptions.add(tracked);
    return handle;
  }

  publish<T>(topic: string, payload: T): number {
    return this.hub.publish(topic, payload);
  }

  subscriberCount(topic?: string): number {
    return this.hub.subscriberCount(topic);
  }

  // -------------------------------------------------------------------------
  // 7 个方法的宿主端实现
  // -------------------------------------------------------------------------

  async openThread(frame: object, threadId: string): Promise<void> {
    const state = this.requireFrame(frame);
    const allowed = state.registration.threads;
    if (allowed && !allowed.includes(threadId)) {
      throw new BridgeFailure(
        "PermissionDenied",
        `thread "${threadId}" is outside this plugin's project scope`,
      );
    }
    await this.options.present.openThread(threadId);
  }

  async notify(frame: object, level: NotifyLevel, message: string): Promise<void> {
    this.requireFrame(frame);
    await this.options.present.notify(level, message);
  }

  async confirm(frame: object, request: ConfirmRequest): Promise<boolean> {
    this.requireFrame(frame);
    return this.options.present.confirm(request);
  }

  context(frame: object): PluginContext {
    return this.contextFor(this.requireFrame(frame).registration);
  }

  // -------------------------------------------------------------------------
  // 内部
  // -------------------------------------------------------------------------

  private contextFor(registration: FrameRegistration): PluginContext {
    return {
      api: registration.binding.uiApi,
      plugin: { ...registration.plugin },
      host: { ...registration.host },
      project: { handle: registration.binding.project },
      activation: registration.binding.activation,
      theme: this.options.theme?.() ?? "light",
    };
  }

  private requireFrame(frame: object): FrameState {
    const state = this.frames.get(frame);
    if (!state) {
      throw new BridgeFailure("PermissionDenied", "frame is not registered with this host");
    }
    return state;
  }

  private takeToken(state: FrameState): boolean {
    const now = this.now();
    if (now - state.windowStartedAt >= 1000) {
      state.windowStartedAt = now;
      state.windowCount = 0;
    }
    if (state.windowCount >= this.limits.maxRequestsPerSecond) return false;
    state.windowCount += 1;
    return true;
  }

  private async snapshotFor(topic: string): Promise<Snapshot> {
    const binding = this.topicBindings.get(topic);
    if (!binding || !this.options.backend.snapshot) {
      throw new BridgeFailure("RecoveryRequired", `no snapshot source for topic "${topic}"`);
    }
    return this.options.backend.snapshot(binding, topic);
  }
}

function fail(requestId: string, kind: ErrorKind, detail: string): BridgeResponse {
  return { ok: false, requestId, error: { kind, detail } };
}

export function toBridgeError(error: unknown): BridgeError {
  if (error instanceof BridgeFailure) return { kind: error.kind, detail: error.detail };
  if (error instanceof Error) return { kind: "Io", detail: error.message };
  return { kind: "Io", detail: String(error) };
}

/** 键序无关的稳定序列化：`{a:1,b:2}` 与 `{b:2,a:1}` 必须视为同一请求。 */
export function stableStringify(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
  if (Array.isArray(value)) return `[${value.map(stableStringify).join(",")}]`;
  const entries = Object.entries(value as Record<string, unknown>)
    .filter(([, v]) => v !== undefined)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${stableStringify(v)}`).join(",")}}`;
}

function defaultToken(): string {
  const bytes = new Uint8Array(24);
  globalThis.crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}
