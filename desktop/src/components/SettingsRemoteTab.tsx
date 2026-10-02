import { useEffect, useRef, useState } from "react";
import QRCode from "qrcode";
import { buildPairUri } from "../lib/pairUri";

/**
 * 桌面「远程访问」设置页。
 *
 * 三件事：铸一枚配对码给手机输、出示中继地址让用户知道该往手机里填什么、
 * 列出/撤销已配对设备。
 *
 * 所有 RPC 都走注入的 `call` 接缝（形如 `RpcClient.request`），组件不碰全局：
 * 单测因此能给出一个假调用器，纯逻辑（错误映射、倒计时、行删除）无需真服务端。
 */

/** `call` 的接缝形状，与 `RpcClient.request` 一致。 */
export type RemoteCall = (method: string, params: unknown) => Promise<unknown>;

export interface SettingsRemoteTabProps {
  /** RPC 接缝；缺省时组件仍渲染（只读演示），但按钮不可用。 */
  call?: RemoteCall;
  /** 中继地址预填（宿主从已持久化的远端配置里读），用户仍可改。 */
  initialRelayUrl?: string;
}

/** 一条 `pair/create` 结果。 */
export interface PairingCodePayload {
  code: string;
  expires_in: number;
}

/** 一台已配对设备（`device/list` 的行）。 */
export interface PairedDeviceRow {
  id: string;
  name: string;
  scope: string;
  created_at?: number;
  last_seen_at?: number;
}

/** 服务端拒绝「非管理员」。桌面 stdio 客户端是 Admin；网络客户端一律得这个码。 */
export const ADMIN_REQUIRED_TEXT = "需要桌面端权限";

/** 无 `call`（宿主没接缝）时的中性提示：不是错误，只是这个面板没接通。 */
export const DISCONNECTED_TEXT = "未连接到桌面服务";

function isAdminRequired(e: unknown): boolean {
  return (e as { code?: unknown } | null)?.code === -32014;
}

/**
 * 把任意抛出错映射成一句给人看的话。
 *
 * `-32014` 单独说「需要桌面端权限」——这是网络客户端（Control scope）的必然结局，
 * 点名原因比回显协议码有用；其余错误优先回显服务端文案，退而求其次 toString。
 */
function errorText(e: unknown): string {
  if (isAdminRequired(e)) return ADMIN_REQUIRED_TEXT;
  const message = (e as { message?: unknown } | null)?.message;
  if (typeof message === "string" && message.length > 0) return message;
  return e instanceof Error ? e.message : String(e);
}

export function SettingsRemoteTab({ call, initialRelayUrl }: SettingsRemoteTabProps) {
  const [relayUrl, setRelayUrl] = useState(initialRelayUrl ?? "");
  const [code, setCode] = useState<string | null>(null);
  const [remaining, setRemaining] = useState(0);
  const [devices, setDevices] = useState<PairedDeviceRow[]>([]);
  const [deviceError, setDeviceError] = useState<string | null>(null);
  const [codeError, setCodeError] = useState<string | null>(null);
  const [minting, setMinting] = useState(false);
  const [revokingId, setRevokingId] = useState<string | null>(null);

  // 倒计时句柄。每次铸新码都先清旧的，卸载时也必须清——否则测试会看到悬挂的定时器。
  const timer = useRef<ReturnType<typeof setInterval> | null>(null);

  const refreshDevices = async () => {
    if (!call) return;
    try {
      const result = (await call("device/list", {})) as {
        devices?: PairedDeviceRow[];
      };
      setDevices(result?.devices ?? []);
      setDeviceError(null);
    } catch (e) {
      setDevices([]);
      setDeviceError(errorText(e));
    }
  };

  useEffect(() => {
    void refreshDevices();
    return () => {
      if (timer.current !== null) clearInterval(timer.current);
    };
    // 仅挂载时拉一次；`call` 变化由宿主重建组件处理。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 配对码旁的二维码：码与中继地址都就绪才渲染。地址为空时退回纯文本码（降级）。
  const [qrSvg, setQrSvg] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    if (code === null || relayUrl.trim().length === 0) {
      setQrSvg(null);
      return;
    }
    QRCode.toString(buildPairUri(relayUrl.trim(), code), { type: "svg", margin: 1 })
      .then((svg) => {
        if (!cancelled) setQrSvg(svg);
      })
      .catch(() => {
        if (!cancelled) setQrSvg(null);
      });
    return () => {
      cancelled = true;
    };
  }, [code, relayUrl]);

  const mint = async () => {
    if (!call) return;
    setMinting(true);
    setCodeError(null);
    try {
      const result = (await call("pair/create", {})) as PairingCodePayload;
      const total = Math.max(0, Math.floor(result.expires_in));
      if (timer.current !== null) clearInterval(timer.current);
      setCode(result.code);
      setRemaining(total);
      timer.current = setInterval(() => {
        setRemaining((n) => {
          if (n <= 1) {
            if (timer.current !== null) clearInterval(timer.current);
            timer.current = null;
            return 0;
          }
          return n - 1;
        });
      }, 1000);
    } catch (e) {
      setCodeError(errorText(e));
    } finally {
      setMinting(false);
    }
  };

  const revoke = async (id: string) => {
    if (!call) return;
    setRevokingId(id);
    setDeviceError(null);
    try {
      const result = (await call("device/revoke", { device_id: id })) as { removed?: boolean };
      // 只有服务端确认删掉才撤行；`removed:false` 说明它还在。
      if (result?.removed) setDevices((rows) => rows.filter((row) => row.id !== id));
    } catch (e) {
      setDeviceError(errorText(e));
    } finally {
      setRevokingId(null);
    }
  };

  return (
    <section className="p-5">
      <h2 className="text-sm font-medium text-fg">远程访问</h2>
      <p className="mt-2 text-xs text-fg-subtle">
        在手机上打开 Yi-Agent，把下面的中继地址与配对码填进配对表单即可连接本机。
      </p>

      <div className="mt-4">
        <button
          type="button"
          onClick={() => void mint()}
          disabled={!call || minting}
          className="rounded-md border border-line-strong px-3 py-1.5 text-sm text-fg-muted hover:text-fg disabled:opacity-50"
        >
          {minting ? "生成中…" : "生成配对码"}
        </button>

        {code !== null && (
          <div
            aria-label="配对码"
            className="mt-3 inline-flex items-center gap-3 rounded-md border border-line bg-surface px-3 py-2"
          >
            <span className="text-lg tracking-widest text-fg">{code}</span>
            <span className="text-xs text-fg-muted">剩余 {remaining} 秒</span>
          </div>
        )}

        {qrSvg !== null && (
          <div
            aria-label="配对二维码"
            className="mt-3 h-40 w-40 [&>svg]:h-full [&>svg]:w-full"
            // QRCode 生成的 SVG 是本地确定性输出（无外部输入拼进标签），故内联安全。
            dangerouslySetInnerHTML={{ __html: qrSvg }}
          />
        )}

        {codeError !== null && (
          <p role="alert" className="mt-2 text-xs text-red-400">
            {codeError}
          </p>
        )}
      </div>

      <label className="mt-4 block text-xs text-fg-muted" htmlFor="remote-relay-url">
        中继地址
        <input
          id="remote-relay-url"
          type="text"
          inputMode="url"
          autoCapitalize="none"
          autoCorrect="off"
          spellCheck={false}
          placeholder="wss://relay.example.com/ws?session=..."
          value={relayUrl}
          onChange={(e) => setRelayUrl(e.target.value)}
          className="mt-1 w-full rounded-md border border-line-strong bg-surface px-3 py-2 text-sm text-fg placeholder:text-fg-faint focus:outline-none"
        />
      </label>

      <div className="mt-5">
        <h3 className="text-sm font-medium text-fg">已配对设备</h3>
        {deviceError !== null && (
          <p role="alert" className="mt-2 text-xs text-red-400">
            {deviceError}
          </p>
        )}
        {devices.length === 0 ? (
          deviceError === null && (
            <p className="mt-2 text-xs text-fg-subtle">
              {call ? "还没有已配对的设备。" : DISCONNECTED_TEXT}
            </p>
          )
        ) : (
          <ul className="mt-2 divide-y divide-line rounded-md border border-line">
            {devices.map((device) => (
              <li key={device.id} className="flex items-center justify-between px-3 py-2">
                <span className="min-w-0 flex-1 truncate text-sm text-fg">{device.name}</span>
                <span className="ml-3 shrink-0 text-xs text-fg-subtle">{device.scope}</span>
                <button
                  type="button"
                  aria-label={`撤销 ${device.name}`}
                  onClick={() => void revoke(device.id)}
                  disabled={!call || revokingId === device.id}
                  className="ml-3 shrink-0 rounded px-2 py-0.5 text-xs text-fg-muted hover:text-fg disabled:opacity-50"
                >
                  撤销
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </section>
  );
}
