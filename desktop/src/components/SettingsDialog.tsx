import { useEffect, useState } from "react";
import type { Theme } from "../lib/theme";
import { SettingsGeneralTab } from "./SettingsGeneralTab";
import { SettingsRemoteTab, type RemoteCall } from "./SettingsRemoteTab";

/** 左侧 Tab 栏。加一个 Tab 只需往这张表里加项，再在下面渲染它的面板。 */
const TABS = [
  { id: "general", label: "通用" },
  { id: "remote", label: "远程访问" },
] as const;

type TabId = (typeof TABS)[number]["id"];

export function SettingsDialog({
  open,
  theme,
  onThemeChange,
  onClose,
  remoteCall,
  relayUrl,
}: {
  open: boolean;
  theme: Theme;
  onThemeChange: (theme: Theme) => void;
  onClose: () => void;
  /** 「远程访问」Tab 的 RPC 接缝，由宿主注入；缺省时该 Tab 只读。 */
  remoteCall?: RemoteCall;
  /** 中继地址预填。 */
  relayUrl?: string;
}) {
  const [active, setActive] = useState<TabId>("general");

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div
        data-settings-backdrop
        className="absolute inset-0 bg-black/50"
        onClick={onClose}
      />
      <div
        role="dialog"
        aria-modal="true"
        aria-label="设置"
        className="relative flex h-[70vh] w-[70vw] max-w-3xl overflow-hidden rounded-lg border border-line bg-panel text-fg shadow-2xl"
      >
        <nav
          role="tablist"
          aria-orientation="vertical"
          className="flex w-40 shrink-0 flex-col border-r border-line bg-surface p-2"
        >
          {TABS.map((t) => (
            <button
              key={t.id}
              role="tab"
              aria-selected={t.id === active}
              onClick={() => setActive(t.id)}
              className="rounded px-3 py-1.5 text-left text-sm text-fg-muted hover:bg-raised/50 aria-selected:text-fg"
            >
              {t.label}
            </button>
          ))}
        </nav>
        <div className="flex min-w-0 flex-1 flex-col">
          <div className="flex items-center justify-between border-b border-line px-4 py-2">
            <span className="text-xs text-fg-subtle">设置</span>
            <button
              type="button"
              aria-label="关闭设置"
              onClick={onClose}
              className="rounded px-2 text-fg-subtle hover:text-fg"
            >
              ×
            </button>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto">
            {active === "general" ? (
              <SettingsGeneralTab theme={theme} onThemeChange={onThemeChange} />
            ) : (
              <SettingsRemoteTab call={remoteCall} initialRelayUrl={relayUrl} />
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
