import { useState, useRef, useEffect, type FormEvent } from "react";
import { type PairedDevice, defaultRedeem } from "../pairing";
import { parsePairUri } from "../lib/pairUri";
import { QrScanner } from "./QrScanner";
import { isIos } from "../lib/platform";
import {
  type StorageLike,
  type RemoteConfig,
  saveRemoteConfig,
  storedRemoteConfig,
} from "../lib/remoteConfig";

/**
 * 手机（iOS）首次启动的配对表单。
 *
 * 与桌面的区别是「无配置即无处可去」：`transportFactory` 在没有持久化远端配置时
 * 退回 `tauriTransport()`，而 iOS 上没有 Tauri IPC——不配对就是死路。所以这一步
 * 必须自己把凭据落盘（`saveRemoteConfig`），配对成功后由宿主重建 transport。
 *
 * 所有副作用都走注入的接缝（`redeem` / `storage`），组件本身不直接摸全局对象：
 * 单测因此能给出假的兑换器与内存 storage，并且默认值只在调用时才求值。
 */

/** 服务端用 4401 表示「码不存在 / 已用过 / 已过期」，三者对客户端不可区分。 */
export const INVALID_CODE_TEXT = "配对码无效或已过期，请在桌面重新生成";
/** 其余失败（网络、TLS、中继不可达……）统一说法，不臆断原因。 */
export const CONNECT_FAILED_TEXT = "无法连接服务器";
/** 扫码结果不是合法配对载荷时的文案。 */
export const NOT_A_PAIRING_QR_TEXT = "不是有效的配对二维码";
/** 相机不可用（拒绝 / 无设备）时的文案；表单仍可手输。 */
export const CAMERA_UNAVAILABLE_TEXT = "无法访问相机，可手输配对码";

/**
 * 从 UA 推断机型名，作为设备名的默认值。
 *
 * iPadOS 13+ 的 Safari 默认请求桌面版站点，UA 长成 `Macintosh … Mobile/…`
 * （真 Mac 桌面浏览器不会带 `Mobile`），所以要连 `Macintosh` + `Mobile` 一起认。
 * 认不出来时给 `"iPhone"`：这是 iOS 构建，设备名只是给人看的标签。
 */
export function inferDeviceName(userAgent?: string): string {
  const ua = userAgent ?? (globalThis.navigator?.userAgent ?? "");
  if (/\biPad\b/i.test(ua)) return "iPad";
  if (/\biPhone\b|\biPod\b/i.test(ua)) return "iPhone";
  if (/\bMacintosh\b/i.test(ua) && /\bMobile\b/i.test(ua)) return "iPad";
  return "iPhone";
}

export interface PairingScreenProps {
  /**
   * 整个提交动作的宿主实现；给了就完全接管（宿主自己负责落盘与后续跳转）。
   * 与 `redeem` 二选一，`onSubmit` 优先。
   */
  onSubmit?: (url: string, code: string, deviceName: string) => Promise<void>;
  /** 中继地址预填（深链接等），用户仍可改。 */
  initialUrl?: string;
  /** 持久化接缝；默认 `localStorage`。 */
  storage?: StorageLike;
  /** 兑换接缝；默认 `defaultRedeem`（中继 URL 走帧，直连走 `?pair=`）。 */
  redeem?: (url: string, code: string, deviceName: string) => Promise<PairedDevice>;
  /** 凭据落盘后回调——宿主据此把 transport 重算成 ws。 */
  onPaired?: () => void;
  /** 是否显示「扫码」按钮；默认仅 iOS（远程构建）。 */
  enableScan?: boolean;
  /**
   * 扫码接缝（测试用）：打开扫描器并把文本/错误回调进来，返回 `stop()`。
   * 缺省时用内建 `QrScanner`（真实相机）。
   */
  scan?: (onText: (t: string) => void, onError: (e: unknown) => void) => () => void;
}

/** 只认 `code === 4401` 的拒绝；文案里带 4401 也算（`pairing.ts` 的拒绝是纯文本）。 */
function isInvalidCode(e: unknown): boolean {
  const code = (e as { code?: unknown } | null)?.code;
  if (code === 4401) return true;
  const message = e instanceof Error ? e.message : (e as { message?: unknown } | null)?.message;
  return typeof message === "string" && message.includes("4401");
}

/** `localStorage` 的默认实现；不可用（隐私模式等）时退化为无操作。 */
function defaultStorage(): StorageLike {
  try {
    const s = globalThis.localStorage;
    if (s) return s as StorageLike;
  } catch {
    // 取全局就抛（某些 webview）——落盘失败是可理解的降级点，不是崩溃点。
  }
  return { getItem: () => null, setItem: () => {}, removeItem: () => {} };
}

export function PairingScreen({
  onSubmit,
  initialUrl,
  storage,
  redeem,
  onPaired,
  enableScan,
  scan,
}: PairingScreenProps) {
  const [url, setUrl] = useState(initialUrl ?? "");
  const [code, setCode] = useState("");
  const [deviceName, setDeviceName] = useState(() => inferDeviceName());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [scanning, setScanning] = useState(false);

  const trimmedUrl = url.trim();
  const trimmedCode = code.trim();
  // 空输入即禁用：地址和码都非空才可能成功，先放行再报错只是浪费一次握手。
  const canSubmit = trimmedUrl.length > 0 && trimmedCode.length > 0 && !busy;
  const canScan = (enableScan ?? isIos()) && !busy;

  /** 实际兑换：表单与扫码两条路径都走它。 */
  const runRedeem = async (u: string, c: string) => {
    const name = deviceName.trim() || inferDeviceName();
    setBusy(true);
    setError(null);
    try {
      if (onSubmit) {
        await onSubmit(u, c, name);
      } else {
        const device = await (redeem ?? defaultRedeem)(u, c, name);
        const config: RemoteConfig = { url: u, token: device.token };
        saveRemoteConfig(storage ?? defaultStorage(), config);
      }
      onPaired?.();
    } catch (err) {
      setError(isInvalidCode(err) ? INVALID_CODE_TEXT : CONNECT_FAILED_TEXT);
    } finally {
      setBusy(false);
      setScanning(false);
    }
  };

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!canSubmit) return;
    await runRedeem(trimmedUrl, trimmedCode);
  };

  /** 扫到一帧文本：解析成功则回填并**自动配对**；失败则提示且扫描器保持打开。 */
  const handleScanText = (text: string) => {
    const parsed = parsePairUri(text);
    if (!parsed) {
      setError(NOT_A_PAIRING_QR_TEXT);
      return; // 扫描器保持打开，可重试
    }
    setUrl(parsed.relay);
    setCode(parsed.code);
    void runRedeem(parsed.relay, parsed.code);
  };

  const handleScanError = () => {
    setError(CAMERA_UNAVAILABLE_TEXT);
    setScanning(false);
  };

  return (
    <div className="flex h-screen items-center justify-center bg-surface p-4 text-fg">
      <form
        aria-label="配对"
        onSubmit={(e) => void submit(e)}
        className="w-full max-w-sm rounded-lg border border-line bg-panel p-5"
      >
        <h1 className="text-sm font-medium text-fg">连接桌面</h1>
        <p className="mt-2 text-xs text-fg-subtle">
          在桌面端「远程访问」生成配对码，把中继地址和码填到这里。
        </p>

        <label className="mt-4 block text-xs text-fg-muted" htmlFor="pairing-url">
          服务器地址
          <input
            id="pairing-url"
            type="text"
            inputMode="url"
            autoCapitalize="none"
            autoCorrect="off"
            spellCheck={false}
            placeholder="wss://relay.example.com/ws?session=..."
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            className="mt-1 w-full rounded-md border border-line-strong bg-surface px-3 py-2 text-sm text-fg placeholder:text-fg-faint focus:outline-none disabled:opacity-50"
          />
        </label>

        <label className="mt-3 block text-xs text-fg-muted" htmlFor="pairing-code">
          配对码
          <input
            id="pairing-code"
            type="text"
            autoCapitalize="characters"
            autoCorrect="off"
            spellCheck={false}
            placeholder="XXXX-XXXX"
            value={code}
            onChange={(e) => setCode(e.target.value)}
            className="mt-1 w-full rounded-md border border-line-strong bg-surface px-3 py-2 text-sm tracking-widest text-fg placeholder:text-fg-faint focus:outline-none disabled:opacity-50"
          />
        </label>

        <label className="mt-3 block text-xs text-fg-muted" htmlFor="pairing-name">
          设备名称
          <input
            id="pairing-name"
            type="text"
            value={deviceName}
            onChange={(e) => setDeviceName(e.target.value)}
            className="mt-1 w-full rounded-md border border-line-strong bg-surface px-3 py-2 text-sm text-fg placeholder:text-fg-faint focus:outline-none disabled:opacity-50"
          />
        </label>

        {error !== null && !scanning && (
          <p role="alert" className="mt-3 text-xs text-red-400">
            {error}
          </p>
        )}

        <button
          type="submit"
          disabled={!canSubmit}
          className="mt-4 w-full rounded-md border border-line-strong px-3 py-2 text-sm text-fg-muted hover:text-fg disabled:opacity-50"
        >
          {busy ? "配对中…" : "配对"}
        </button>

        {canScan && (
          <button
            type="button"
            onClick={() => {
              setError(null);
              setScanning(true);
            }}
            className="mt-3 w-full rounded-md border border-line-strong px-3 py-2 text-sm text-fg-muted hover:text-fg"
          >
            扫码
          </button>
        )}

        {scanning && (
          <div
            role="dialog"
            aria-label="扫描二维码"
            className="fixed inset-0 z-50 flex flex-col bg-black"
          >
            <div className="flex items-center justify-between px-4 py-3">
              <span className="text-xs text-white/70">对准桌面端显示的二维码</span>
              <button
                type="button"
                onClick={() => setScanning(false)}
                className="rounded px-2 py-1 text-xs text-white/80 hover:text-white"
              >
                取消
              </button>
            </div>
            <div className="relative min-h-0 flex-1">
              {scan ? (
                <ScanHost scan={scan} onText={handleScanText} onError={handleScanError} />
              ) : (
                <QrScanner onText={handleScanText} onError={handleScanError} />
              )}
              {error !== null && (
                <p
                  role="alert"
                  className="absolute inset-x-0 bottom-6 mx-auto w-fit rounded bg-black/70 px-3 py-1 text-xs text-red-300"
                >
                  {error}
                </p>
              )}
            </div>
          </div>
        )}
      </form>
    </div>
  );
}

/**
 * 把注入的 `scan` 接缝适配成一个挂载即开启的组件。
 *
 * 回调放进 ref 再交给 effect：`scan` 的 identity 稳定（只随 prop 变），effect 不会
 * 因为父组件每次渲染新建回调而反复开关扫描器。
 */
function ScanHost({
  scan,
  onText,
  onError,
}: {
  scan: NonNullable<PairingScreenProps["scan"]>;
  onText: (t: string) => void;
  onError: (e: unknown) => void;
}) {
  const ref = useRef({ onText, onError });
  ref.current = { onText, onError };
  useEffect(() => scan((t) => ref.current.onText(t), (e) => ref.current.onError(e)), [scan]);
  return null;
}

/**
 * 宿主（`App`）用的判定：iOS 且没有任何已持久化的远端配置 → 必须先配对。
 *
 * `storedRemoteConfig` 对损坏/缺失的值一律返回 null，所以「读不出来」与「没配过」
 * 走同一条路：进配对表单，而不是掉进无 Tauri 的死路。
 */
export function needsPairing(isIos: boolean, storage: StorageLike = defaultStorage()): boolean {
  return isIos && storedRemoteConfig(storage) === null;
}
