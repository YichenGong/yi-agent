import { useEffect, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import type { Theme } from "../lib/theme";
import { SettingsGeneralTab } from "./SettingsGeneralTab";
import { SettingsRemoteTab, type RemoteCall } from "./SettingsRemoteTab";

/** 左侧 Tab 栏。加一个 Tab 只需往这张表里加项，再在下面渲染它的面板。 */
const TABS = [
  { id: "general", label: "通用" },
  { id: "remote", label: "远程访问" },
] as const;

type TabId = (typeof TABS)[number]["id"];

/** Tab 与面板的 id 约定：`aria-controls`/`aria-labelledby` 靠它俩对上。 */
const tabId = (id: TabId) => `settings-tab-${id}`;
const panelId = (id: TabId) => `settings-panel-${id}`;

export function SettingsDialog({
  open,
  theme,
  onThemeChange,
  onClose,
  remoteCall,
  relayUrl,
  saveRelayUrl,
}: {
  open: boolean;
  theme: Theme;
  onThemeChange: (theme: Theme) => void;
  onClose: () => void;
  /** 「远程访问」Tab 的 RPC 接缝，由宿主注入；缺省时该 Tab 只读。 */
  remoteCall?: RemoteCall;
  /** 中继地址预填。 */
  relayUrl?: string;
  /**
   * 「远程访问」Tab 保存侧车中继地址的接缝（桌面宿主接到 Tauri 的
   * `set_relay_url`）。缺省时保存按钮不可用（iOS 构建没有该 Tauri 命令）。
   */
  saveRelayUrl?: (value: string | null) => Promise<void>;
}) {
  const [active, setActive] = useState<TabId>("general");
  // roving tabIndex：只有选中的 Tab 是 Tab 停靠点，↑↓←→ 在表内移动。
  const tabRefs = useRef<Partial<Record<TabId, HTMLButtonElement | null>>>({});
  // 打开前的焦点归处，关闭时原样还给它（通常是那个「设置」触发按钮）。
  const restoreRef = useRef<HTMLElement | null>(null);
  // 打开瞬间要落焦的 Tab：只认「本次打开」的选区，避免选区一变就重跑打开副作用。
  const activeAtOpen = useRef<TabId>("general");
  activeAtOpen.current = active;

  useEffect(() => {
    if (!open) return;
    restoreRef.current =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;
    // 落焦在选中的 Tab 上：键盘用户一进来就在 tablist 里，箭头键随即可用。
    tabRefs.current[activeAtOpen.current]?.focus();
    // 关闭（open 转 false）或卸载时把焦点还给来处。
    return () => restoreRef.current?.focus();
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!open) return null;

  /** 表内箭头/Home/End 导航：移动选区并同步把焦点带过去。 */
  const onTablistKeyDown = (e: KeyboardEvent) => {
    const i = TABS.findIndex((t) => t.id === active);
    // aria-orientation="vertical"，但左右键同样惯例可用。
    let next = i;
    if (e.key === "ArrowDown" || e.key === "ArrowRight") next = (i + 1) % TABS.length;
    else if (e.key === "ArrowUp" || e.key === "ArrowLeft") next = (i - 1 + TABS.length) % TABS.length;
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = TABS.length - 1;
    else return;
    e.preventDefault();
    const id = TABS[next].id;
    setActive(id);
    tabRefs.current[id]?.focus();
  };

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
          onKeyDown={onTablistKeyDown}
          className="flex w-40 shrink-0 flex-col border-r border-line bg-surface p-2"
        >
          {TABS.map((t) => (
            <button
              key={t.id}
              id={tabId(t.id)}
              ref={(el) => {
                tabRefs.current[t.id] = el;
              }}
              role="tab"
              aria-selected={t.id === active}
              aria-controls={panelId(t.id)}
              tabIndex={t.id === active ? 0 : -1}
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
          {/* 一次只渲染选中的面板；id/aria-labelledby 与上面的 Tab 成对。 */}
          <div
            role="tabpanel"
            id={panelId(active)}
            aria-labelledby={tabId(active)}
            className="min-h-0 flex-1 overflow-y-auto"
          >
            {active === "general" ? (
              <SettingsGeneralTab theme={theme} onThemeChange={onThemeChange} />
            ) : (
              <SettingsRemoteTab
                call={remoteCall}
                initialRelayUrl={relayUrl}
                saveRelayUrl={saveRelayUrl}
              />
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
