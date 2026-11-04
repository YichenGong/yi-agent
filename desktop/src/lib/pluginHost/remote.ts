/**
 * 远端宿主身份：桌面侧对 `assetRelease` 的回答做绑定判定。
 *
 * # 为什么身份要单独成一个模块
 *
 * 桌面客户端可以同时连着本机和若干远端机器，还可能开着两个远端连接。同一个
 * 插件在 A 上是 1.2.0、在 B 上是 1.3.0 是常态。如果视图身份只有"插件 + 视图
 * 名"，切到 B 之后完全可能展示 A 的包：A 的页面读，B 的授权写，中间没有任何
 * 东西拦着。
 *
 * 所以绑定键必须同时包含 **宿主实例 + 完整发布身份 + 激活代次**。少一样就有
 * 一个"看起来一样"的混淆面。
 *
 * # 它不做什么
 *
 * 不下载、不验签、不写缓存目录——那些在 Rust 侧（`plugin_host::remote_assets`）。
 * 这里只回答"这个视图现在能不能显示"，让 UI 能在不猜测的情况下说清原因。
 */

import type { BridgeError } from "./types";

/** 宿主身份。与 Rust 的 `HostIdentity` 同形（serde camelCase）。 */
export interface HostIdentity {
  /** `local` 或 `remote:<profile id>`。 */
  kind: string;
  instanceId: string;
  /** 宿主上那次会话的激活代次。代次变化 = 旧视图失效。 */
  generation: number;
}

/** `plugins/v1/assetRelease` 的结果。**没有** URL、**没有**凭据。 */
export interface ReleaseIdentity {
  plugin: string;
  version: string;
  target: string;
  sha256: string;
  sizeBytes: number;
  publishedAt: number;
  expiresAt: number;
  serial: number;
  authenticated: boolean;
  transfer: { handle: string; kind: "host-mediated" };
  directUrl: null;
  credentials: null;
}

/** 一个已打开的视图及其绑定。 */
export interface ViewBinding {
  instanceId: string;
  host: HostIdentity;
  release: { plugin: string; version: string; target: string; sha256: string };
  activation: number;
  view: string;
}

/** 显示判定的结果。三态而不是布尔：UI 要说清"该重连"还是"该重开"。 */
export type ViewDecision =
  | { show: true }
  | {
      show: false;
      reason: "foreign-host" | "stale-generation" | "foreign-release" | "incompatible-ui" | "not-cached";
      detail: string;
    };

export function isRemote(host: HostIdentity): boolean {
  return host.kind.startsWith("remote:");
}

/**
 * 绑定键。**必须**包含宿主与代次：只含发布身份时，两个宿主上同一版本会被当成
 * 同一个绑定。
 */
export function bindingKey(binding: {
  host: HostIdentity;
  release: { plugin: string; version: string; target: string; sha256: string };
}): string {
  const { host, release } = binding;
  return [
    host.kind,
    host.instanceId,
    String(host.generation),
    release.plugin,
    release.version,
    release.target,
    release.sha256,
  ].join("|");
}

/** 两个宿主身份是否完全相同（含代次）。 */
export function sameHost(a: HostIdentity, b: HostIdentity): boolean {
  return a.kind === b.kind && a.instanceId === b.instanceId && a.generation === b.generation;
}

/**
 * 这个视图现在能不能显示。
 *
 * 判据顺序是有意的：先看宿主（切走了就什么都不用谈），再看代次（同一宿主上
 * 激活被换掉），最后看发布身份（同版本不同摘要的包是另一个东西）。顺序错了
 * 只会给出更含糊的提示，不会放行——但正确顺序能让用户知道该做什么。
 */
export function decideView(options: {
  binding: ViewBinding;
  activeHost: HostIdentity;
  /** 宿主当前的已提交激活。`null` 表示宿主报告没有已提交版本。 */
  committed: { version: string; sha256: string; activation: number } | null;
  /** 本地界面 API 版本。 */
  localUiApi: string;
  /** 宿主要求的界面 API 版本。 */
  requiredUiApi: string;
  /** 是否已有匹配且校验通过的缓存。 */
  cached: boolean;
}): ViewDecision {
  const { binding, activeHost, committed, localUiApi, requiredUiApi, cached } = options;

  if (!sameHost(binding.host, activeHost)) {
    return {
      show: false,
      reason: "foreign-host",
      detail: `this view is bound to ${binding.host.kind}#${binding.host.instanceId} generation ${binding.host.generation}, but the active host is ${activeHost.kind}#${activeHost.instanceId} generation ${activeHost.generation}`,
    };
  }

  if (committed === null) {
    return {
      show: false,
      reason: "stale-generation",
      detail: `the plugin has no committed activation on ${activeHost.instanceId}; reopen the view after activating it`,
    };
  }
  if (
    committed.activation !== binding.activation ||
    committed.version !== binding.release.version ||
    committed.sha256 !== binding.release.sha256
  ) {
    return {
      show: false,
      reason: "stale-generation",
      detail: `this view is bound to ${binding.release.version}@${binding.release.sha256.slice(0, 8)} activation ${binding.activation}, but the host has ${committed.version}@${committed.sha256.slice(0, 8)} activation ${committed.activation}`,
    };
  }

  // 界面 API 不兼容只禁**这个视图**，不影响远端已有任务继续跑（设计 §10）。
  if (requiredUiApi !== localUiApi) {
    return {
      show: false,
      reason: "incompatible-ui",
      detail: `the release needs plugin UI API ${requiredUiApi}, this app speaks ${localUiApi}; upgrade the app — falling back to another version's UI is not allowed`,
    };
  }

  if (!cached) {
    return {
      show: false,
      reason: "not-cached",
      detail: "no verified cached package matches this release, and the host is unreachable; arbitrary older bytes are not a fallback",
    };
  }

  return { show: true };
}

/**
 * `assetRelease` 的回答能不能当作"认证过的发布身份"。
 *
 * 任何一条不满足就拒绝，而不是"尽力而为"地放行：一个被接受但校验宽松的身份
 * 会让后面的缓存命中变成一次未验证的读取。
 */
export function acceptRelease(value: unknown): { ok: true; release: ReleaseIdentity } | { ok: false; error: BridgeError } {
  if (typeof value !== "object" || value === null) {
    return { ok: false, error: { kind: "InvalidManifest", detail: "assetRelease returned a non-object" } };
  }
  const candidate = value as Partial<ReleaseIdentity>;
  const fail = (detail: string): { ok: false; error: BridgeError } => ({
    ok: false,
    error: { kind: "InvalidManifest", detail },
  });

  if (candidate.authenticated !== true) {
    return fail("the release identity is not authenticated");
  }
  for (const field of ["plugin", "version", "target", "sha256"] as const) {
    const v = candidate[field];
    if (typeof v !== "string" || v.length === 0) {
      return fail(`assetRelease is missing \`${field}\``);
    }
  }
  if (typeof candidate.sha256 !== "string" || !/^[0-9a-f]{64}$/i.test(candidate.sha256)) {
    return fail("sha256 must be a 64-character hex digest");
  }
  if (!candidate.transfer || candidate.transfer.kind !== "host-mediated" || typeof candidate.transfer.handle !== "string") {
    return fail("a release must come with a host-mediated transfer handle");
  }
  // 这两条是"桥是唯一业务通道"的机械保证。出现了就不是身份，是凭据泄漏。
  if (candidate.directUrl !== null && candidate.directUrl !== undefined) {
    return fail("a release identity must not carry a direct URL");
  }
  if (candidate.credentials !== null && candidate.credentials !== undefined) {
    return fail("a release identity must not carry credentials");
  }
  return { ok: true, release: candidate as ReleaseIdentity };
}

/**
 * 一次连接切换之后，哪些视图必须关掉。
 *
 * 返回**全部**绑定到旧宿主的实例：桌面同一时刻只看一个宿主，旧窗口留着会让
 * 读打到 A、写落到 B。这里不做"保留看起来一致的"优化——那种优化正是混淆的来源。
 */
export function revokeOnSwitch(bindings: readonly ViewBinding[], next: HostIdentity): string[] {
  return bindings.filter((binding) => !sameHost(binding.host, next)).map((binding) => binding.instanceId);
}
