import { useCallback, useEffect, useState } from "react";

/**
 * 「远程项目」设置面板：增删 ssh 远端项目、看连接状态、手动重连。
 *
 * 三条设计约束都来自 spec：
 * 1. **闸门关时整个面板不渲染**（§5.3.1）——`remote_enabled` 为 false 就
 *    `return null`，不是渲染一个灰掉的表单。dev-only 功能在 stable 上不该有
 *    任何痕迹。
 * 2. **断线要给出可读原因 + 手动重连**（§4.3 第 2 条）——远端 5 次连续失败后
 *    停止自动重试，此时如果只说「已断开」，用户唯一的选项是无意义地等待。
 * 3. **`instance_root` 不由用户输入**（§4.9）——它按 profile id 派生，让用户
 *    填一个绝对路径只会产出「看起来对但互相覆盖」的目录。
 */

export interface RemoteProfileView {
  id: string;
  label: string;
  host: string;
  local_path: string;
  remote_path: string;
  remote_arch: string;
  binary_version: string;
}

interface SlotState {
  status: "connected" | "connecting" | "disconnected";
  lastError: string | null;
}

/** 宿主 IPC 接缝。可注入是为了让组件在测试里不碰真实 Tauri。 */
export type RemoteInvoke = <T = unknown>(
  command: string,
  args?: Record<string, unknown>,
) => Promise<T>;

/**
 * 真实的 Tauri `invoke`，懒加载。
 *
 * 动态 import 而非顶层 import：`@tauri-apps/api` 在非 Tauri 环境（浏览器里跑
 * 前端、vitest 默认 node 环境）会抛，而 `SettingsDialog` 在那种环境下也要能
 * 渲染出其余 Tab。
 */
export const defaultHostInvoke: RemoteInvoke = async <T,>(
  command: string,
  args?: Record<string, unknown>,
) => {
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(command, args);
};

/** 三个受支持的 target（spec §2.4.5），顺序即下拉框顺序。 */
const ARCHES = [
  { value: "aarch64-apple-darwin", label: "macOS (Apple Silicon)" },
  { value: "x86_64-apple-darwin", label: "macOS (Intel)" },
  { value: "x86_64-unknown-linux-gnu", label: "Linux (x86_64)" },
] as const;

const emptyDraft = (): RemoteProfileDraft => ({
  id: "",
  label: "",
  host: "",
  local_path: "",
  remote_path: "",
  remote_arch: ARCHES[2].value,
  binary_version: "",
});

export interface RemoteProfileDraft extends Omit<RemoteProfileView, never> {}

export function RemoteSettings({ invoke }: { invoke: RemoteInvoke }) {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [profiles, setProfiles] = useState<RemoteProfileView[]>([]);
  const [slots, setSlots] = useState<Record<string, SlotState>>({});
  const [draft, setDraft] = useState<RemoteProfileDraft | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    const on = await invoke<boolean>("remote_enabled");
    setEnabled(on);
    if (!on) return;
    const listed = await invoke<{
      enabled: boolean;
      profiles: RemoteProfileView[];
    }>("remote_list");
    setProfiles(listed.profiles ?? []);
    try {
      const state = await invoke<{ slots: Record<string, SlotState> }>(
        "remote_status",
      );
      setSlots(state.slots ?? {});
    } catch {
      // `remote_status` 只在闸门开时可用；取不到就当全部未知，不炸整个面板。
      setSlots({});
    }
  }, [invoke]);

  useEffect(() => {
    void refresh().catch((e) => setError(String(e)));
  }, [refresh]);

  // 闸门未知（首帧）与关闭都返回 null：多渲染一帧表单再撤掉会造成闪烁。
  if (enabled !== true) return null;

  const act = async (label: string, run: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await run();
      await refresh();
    } catch (e) {
      setError(`${label}失败：${String(e)}`);
    } finally {
      setBusy(false);
    }
  };

  const save = () =>
    act("保存", async () => {
      if (!draft) return;
      await invoke("remote_upsert", { profile: draft });
      setDraft(null);
    });

  return (
    <section className="flex flex-col gap-4">
      <header className="flex items-center justify-between gap-2">
        <h3 className="text-sm font-medium">远程项目</h3>
        <button
          type="button"
          className="rounded border border-border px-2 py-1 text-xs"
          disabled={busy}
          onClick={() => setDraft(emptyDraft())}
        >
          新增
        </button>
      </header>

      {error !== null ? (
        <p role="alert" className="text-xs text-danger">
          {error}
        </p>
      ) : null}

      {profiles.length === 0 ? (
        <p className="text-xs text-fg-subtle">
          还没有远端项目。新增后会经 ssh 在那台机器上跑一个常驻 yi-agent。
        </p>
      ) : (
        <ul className="flex flex-col gap-3">
          {profiles.map((profile) => (
            <RemoteRow
              key={profile.id}
              profile={profile}
              slot={slots[profile.id]}
              busy={busy}
              onEdit={() => setDraft({ ...profile })}
              onRemove={() =>
                act("删除", async () => {
                  await invoke("remote_remove", { profileId: profile.id });
                })
              }
              onConnect={() =>
                act("连接", async () => {
                  await invoke("remote_connect", { profileId: profile.id });
                })
              }
              onDisconnect={() =>
                act("断开", async () => {
                  await invoke("remote_disconnect", { profileId: profile.id });
                })
              }
            />
          ))}
        </ul>
      )}

      {draft !== null ? (
        <ProfileForm
          draft={draft}
          onChange={setDraft}
          busy={busy}
          onSave={save}
          onCancel={() => setDraft(null)}
        />
      ) : null}
    </section>
  );
}

function RemoteRow({
  profile,
  slot,
  busy,
  onEdit,
  onRemove,
  onConnect,
  onDisconnect,
}: {
  profile: RemoteProfileView;
  slot: SlotState | undefined;
  busy: boolean;
  onEdit: () => void;
  onRemove: () => void;
  onConnect: () => void;
  onDisconnect: () => void;
}) {
  const status = slot?.status ?? "disconnected";
  const reason = slot?.lastError ?? null;
  return (
    <li className="rounded border border-border p-3">
      <div className="flex items-center justify-between gap-2">
        <div className="min-w-0">
          <p className="truncate text-sm">{profile.label || profile.id}</p>
          <p className="truncate text-xs text-fg-subtle">
            {profile.host} · {profile.remote_path}
          </p>
        </div>
        <StatusPill status={status} />
      </div>

      {/* 断线时必须给出原因：只说「已断开」用户唯一能做的只有等待。 */}
      {status !== "connected" && reason !== null ? (
        <p className="mt-2 break-words text-xs text-danger">{reason}</p>
      ) : null}

      <div className="mt-2 flex flex-wrap gap-2">
        {status === "connected" ? (
          <button
            type="button"
            className="rounded border border-border px-2 py-1 text-xs"
            disabled={busy}
            onClick={onDisconnect}
          >
            断开
          </button>
        ) : (
          <button
            type="button"
            className="rounded border border-border px-2 py-1 text-xs"
            disabled={busy}
            onClick={onConnect}
          >
            重连
          </button>
        )}
        <button
          type="button"
          className="rounded border border-border px-2 py-1 text-xs"
          disabled={busy}
          onClick={onEdit}
        >
          编辑
        </button>
        <button
          type="button"
          className="rounded border border-border px-2 py-1 text-xs"
          disabled={busy}
          onClick={onRemove}
        >
          删除
        </button>
      </div>
    </li>
  );
}

function StatusPill({ status }: { status: SlotState["status"] }) {
  const text =
    status === "connected"
      ? "已连接"
      : status === "connecting"
        ? "连接中"
        : "已断开";
  return (
    <span className="shrink-0 rounded bg-surface px-2 py-0.5 text-xs">
      {text}
    </span>
  );
}

function ProfileForm({
  draft,
  onChange,
  busy,
  onSave,
  onCancel,
}: {
  draft: RemoteProfileDraft;
  onChange: (next: RemoteProfileDraft) => void;
  busy: boolean;
  onSave: () => void;
  onCancel: () => void;
}) {
  const field = (key: keyof RemoteProfileDraft) => ({
    value: draft[key],
    onChange: (event: { target: { value: string } }) =>
      onChange({ ...draft, [key]: event.target.value }),
  });

  return (
    <form
      className="flex flex-col gap-2 rounded border border-border p-3"
      onSubmit={(event) => {
        event.preventDefault();
        onSave();
      }}
    >
      <label className="text-xs">
        标识（小写字母/数字/连字符，用作目录名）
        <input
          className="mt-1 w-full rounded border border-border bg-transparent px-2 py-1"
          {...field("id")}
        />
      </label>
      <label className="text-xs">
        显示名
        <input
          className="mt-1 w-full rounded border border-border bg-transparent px-2 py-1"
          {...field("label")}
        />
      </label>
      <label className="text-xs">
        ssh 目标（`user@host`，可用 ~/.ssh/config 里的别名）
        <input
          className="mt-1 w-full rounded border border-border bg-transparent px-2 py-1"
          {...field("host")}
        />
      </label>
      <label className="text-xs">
        本机目录（这台机器上的项目路径）
        <input
          className="mt-1 w-full rounded border border-border bg-transparent px-2 py-1"
          {...field("local_path")}
        />
      </label>
      <label className="text-xs">
        远端目录（那台机器上的项目路径）
        <input
          className="mt-1 w-full rounded border border-border bg-transparent px-2 py-1"
          {...field("remote_path")}
        />
      </label>
      <label className="text-xs">
        远端平台
        <select
          className="mt-1 w-full rounded border border-border bg-transparent px-2 py-1"
          value={draft.remote_arch}
          onChange={(event) =>
            onChange({ ...draft, remote_arch: event.target.value })
          }
        >
          {ARCHES.map((arch) => (
            <option key={arch.value} value={arch.value}>
              {arch.label}
            </option>
          ))}
        </select>
      </label>
      <label className="text-xs">
        远端二进制版本（留空则用本机版本）
        <input
          className="mt-1 w-full rounded border border-border bg-transparent px-2 py-1"
          {...field("binary_version")}
        />
      </label>
      <div className="flex gap-2">
        <button
          type="submit"
          className="rounded border border-border px-2 py-1 text-xs"
          disabled={busy}
        >
          保存
        </button>
        <button
          type="button"
          className="rounded border border-border px-2 py-1 text-xs"
          onClick={onCancel}
        >
          取消
        </button>
      </div>
    </form>
  );
}
