import { useEffect, useRef, useState } from "react";
import { Modal } from "../ui/Modal";
import type { KeyboardEvent } from "react";
import type { Theme } from "../lib/theme";
import { SettingsGeneralTab } from "./SettingsGeneralTab";
import { SettingsRemoteTab, type RemoteCall } from "./SettingsRemoteTab";
import { SettingsPluginsTab } from "./SettingsPluginsTab";
import { SettingsModelsTab } from "./SettingsModelsTab";
import { SettingsSkillsTab } from "./SettingsSkillsTab";
import { SettingsMcpTab } from "./SettingsMcpTab";
import type { McpCall } from "../lib/mcp";
import type { SkillsCall } from "../lib/skills";
import { SettingsTagsTab } from "./SettingsTagsTab";
import { SettingsFlagsTab, type FlagsCall } from "./SettingsFlagsTab";
import { DEFAULT_TAG_NAMES, type TagId } from "../lib/threadTags";

/** 左侧 Tab 栏。加一个 Tab 只需往这张表里加项，再在下面渲染它的面板。 */
const TABS = [
  { id: "general", label: "通用" },
  { id: "remote", label: "远程访问" },
  { id: "plugins", label: "插件" },
  { id: "models", label: "模型" },
  { id: "skills", label: "技能" },
  { id: "mcp", label: "MCP" },
  { id: "tags", label: "标签" },
  // 只有 dev 构建可见；过滤在渲染期做（见 `visibleTabs`），常量表保持完整。
  { id: "flags", label: "实验功能" },
] as const;

type TabId = (typeof TABS)[number]["id"];

/** Tab 与面板的 id 约定：`aria-controls`/`aria-labelledby` 靠它俩对上。 */
const tabId = (id: TabId) => `settings-tab-${id}`;
const panelId = (id: TabId) => `settings-panel-${id}`;

export function SettingsDialog({
  open,
  isMobile = false,
  theme,
  onThemeChange,
  watchmanEnabled,
  onToggleWatchman,
  watchmanWarning,
  onClose,
  remoteCall,
  pluginCall,
  modelCall,
  skillsCall,
  skillsProjects,
  skillsDefaultProject,
  mcpCall,
  mcpProjects,
  mcpDefaultProject,
  pluginProjects,
  pluginDefaultProject,
  relayUrl,
  saveRelayUrl,
  tagNames,
  saveTagName,
  buildKind,
  flagsCall,
}: {
  open: boolean;
  /**
   * 手机端（配对后的 iPhone 客户端）：居中弹窗改成整屏页面。由宿主把已有的
   * `useIsMobile()` 结论传下来，本组件不重复探测——`<html data-mobile>` 只能有
   * 一个设置者，两处 effect 会重复写 `dataset`。
   *
   * 只影响两件事：导航方向（`aria-orientation`）与「模型」Tab 的表格/卡片形态。
   * 其余外观覆写全在 `index.css` 的 `[data-mobile="true"]` 前缀规则里，桌面不匹配。
   */
  isMobile?: boolean;
  theme: Theme;
  onThemeChange: (theme: Theme) => void;
  /**
   * 宿主级「后台值守」。从看板面板迁到「通用」：它装的是通用登录项，服务所有
   * 已登记的项目，与当时选中的看板无关。
   */
  watchmanEnabled?: boolean;
  onToggleWatchman?: (next: boolean) => void;
  /** 值守安装/卸载失败的原因；空即无异常。 */
  watchmanWarning?: string | null;
  onClose: () => void;
  /** 「远程访问」Tab 的 RPC 接缝，由宿主注入；缺省时该 Tab 只读。 */
  remoteCall?: RemoteCall;
  /** 「插件」Tab 的宿主 RPC 接缝（plugins/list、plugin/settings/*）。 */
  pluginCall?: (method: string, params: unknown) => Promise<unknown>;
  /** 「模型」Tab 的宿主 RPC 接缝（model/list、model/upsert、...）。 */
  modelCall?: (method: string, params: unknown) => Promise<unknown>;
  /** 「技能」Tab 的宿主 RPC 接缝（skill/list、skill/read）。 */
  skillsCall?: SkillsCall;
  /** 「技能」Tab 可选的配置项目（绝对路径）。 */
  skillsProjects?: string[];
  /** 「技能」Tab 首选项目（当前对话的工作目录）。 */
  skillsDefaultProject?: string;
  /** 「插件」Tab 可选的配置项目（绝对路径）。 */
  pluginProjects?: string[];
  /** 「插件」Tab 首选项目（当前对话的工作目录）。 */
  pluginDefaultProject?: string;
  /** 「MCP」Tab 的宿主 RPC 接缝（mcp/list）。 */
  mcpCall?: McpCall;
  /** 「MCP」Tab 可选的配置项目（绝对路径）。 */
  mcpProjects?: string[];
  /** 「MCP」Tab 首选项目（当前对话的工作目录）。 */
  mcpDefaultProject?: string;
  /** 中继地址预填。 */
  relayUrl?: string;
  /**
   * 「远程访问」Tab 保存侧车中继地址的接缝（桌面宿主接到 Tauri 的
   * `set_relay_url`）。缺省时保存按钮不可用（iOS 构建没有该 Tauri 命令）。
   */
  saveRelayUrl?: (value: string | null) => Promise<void>;
  /** 已解析的标签显示名（6 键齐全）。缺省时「标签」Tab 只读回退默认名。 */
  tagNames?: Record<TagId, string>;
  /** 「标签」Tab 保存单个颜色显示名的接缝（写 ui/settings/write）。 */
  saveTagName?: (tagId: TagId, name: string) => Promise<void> | void;
  /**
   * 宿主经 `build/info` 得到的构建类型。只有 `"dev"` 才渲染「实验功能」Tab：
   * stable 上 `flags/*` 这些方法根本不存在（-32601），渲染出来的面板只会报错。
   */
  buildKind?: "dev" | "release" | "nogit";
  /** 「实验功能」Tab 的 RPC 接缝（flags/list、flags/set）。 */
  flagsCall?: FlagsCall;
}) {
  /**
   * 手机端的两级导航位置。桌面端恒为 `"panel"`（从不渲染分类列表）。
   *
   * 手机上分类可能有 5 个以上，等宽横条在 320pt 上放不下（第 5 格只剩 64px，
   * 「远程访问」四字溢出）。列表 → 子页的结构把横向宽度从约束里摘掉。
   */
  /**
   * 实际渲染的 Tab 表。`flags` 依赖宿主告诉我们的构建类型——不能靠「请求失败就
   * 隐藏」这种事后判断，那会让面板先闪一次错误。
   */
  const visibleTabs = TABS.filter((tab) => tab.id !== "flags" || buildKind === "dev");
  const [active, setActive] = useState<TabId>("general");
  const [phonePane, setPhonePane] = useState<"list" | "panel">("panel");
  /** 从子页返回后要重新落焦的分类。`null` = 无需落焦。 */
  const [pendingFocus, setPendingFocus] = useState<TabId | null>(null);
  /** 分类行的 DOM 句柄，返回时把焦点还给对应行。 */
  const categoryRefs = useRef<Partial<Record<TabId, HTMLButtonElement | null>>>({});
  /** 子页的返回按钮，进入子页时落焦于此。 */
  const backRef = useRef<HTMLButtonElement | null>(null);
  // roving tabIndex：只有选中的 Tab 是 Tab 停靠点，↑↓←→ 在表内移动。
  const tabRefs = useRef<Partial<Record<TabId, HTMLButtonElement | null>>>({});
  // 打开前的焦点归处，关闭时原样还给它（通常是那个「设置」触发按钮）。
  const restoreRef = useRef<HTMLElement | null>(null);
  // 打开瞬间要落焦的 Tab：只认「本次打开」的选区，避免选区一变就重跑打开副作用。
  const activeAtOpen = useRef<TabId>("general");
  activeAtOpen.current = active;

  useEffect(() => {
    if (!open) return;
    // 桌面端恒为 "panel"（从不渲染分类列表）；手机端每次打开都从分类列表起步——
    // 上次停在哪个子页与本次打开无关，留着会让人以为设置只有那一页。
    setPhonePane(isMobile ? "list" : "panel");
    restoreRef.current =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;
    // 落焦在选中的 Tab 上：键盘用户一进来就在 tablist 里，箭头键随即可用。
    tabRefs.current[activeAtOpen.current]?.focus();
    // 关闭（open 转 false）或卸载时把焦点还给来处。
    return () => restoreRef.current?.focus();
  }, [open, isMobile]);
  // 手机端进子页后落焦在「返回」：这是该页唯一的导航控件，键盘/读屏用户一进来就能
  // 退回去。桌面端没有返回按钮，该 effect 自然不生效。
  useEffect(() => {
    if (!open || !isMobile || phonePane !== "panel") return;
    backRef.current?.focus();
  }, [open, isMobile, phonePane, active]);

  // 返回列表后把焦点还给来源行。用 effect 而不是在 onClick 里直接 focus：那一刻该行
  // 还没挂载。`pendingFocus` 用完即清，故 effect 不会重复触发。掉到 body 上会让
  // 键盘/读屏用户丢掉位置。
  useEffect(() => {
    if (pendingFocus === null) return;
    categoryRefs.current[pendingFocus]?.focus();
    setPendingFocus(null);
  }, [pendingFocus]);

  if (!open) return null;

  /** 表内箭头/Home/End 导航：移动选区并同步把焦点带过去。 */
  const onTablistKeyDown = (e: KeyboardEvent) => {
    const i = visibleTabs.findIndex((t) => t.id === active);
    if (i < 0) return;
    // aria-orientation="vertical"，但左右键同样惯例可用。
    let next = i;
    if (e.key === "ArrowDown" || e.key === "ArrowRight") next = (i + 1) % visibleTabs.length;
    else if (e.key === "ArrowUp" || e.key === "ArrowLeft")
      next = (i - 1 + visibleTabs.length) % visibleTabs.length;
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = visibleTabs.length - 1;
    else return;
    e.preventDefault();
    const id = visibleTabs[next].id;
    setActive(id);
    tabRefs.current[id]?.focus();
  };


  return (
    <Modal
      open={open}
      onClose={onClose}
      overlayClassName="fixed inset-0 z-50 flex items-center justify-center"
      // 该 backdrop 必须留在遮罩内、面板前：手机上由 CSS 隐藏，也是测试的选择器。
      overlayBackdrop={
        <div data-settings-backdrop className="absolute inset-0 bg-black/50" onClick={onClose} />
      }
      ariaLabel="设置"
      panelClassName="settings-shell relative flex h-[70vh] w-[70vw] max-w-3xl overflow-hidden rounded-lg border border-line bg-panel text-fg shadow-2xl"
    >
      {/*
        桌面：左侧竖栏 tablist（`aria-orientation="vertical"` + roving tabIndex）。
        手机：不渲染 tablist——分类列表与子页二选一。手机上留一个隐藏 tablist 会与
        可见列表争可访问名，读屏会念出两套导航。
      */}
      {!isMobile && (
        <nav
          role="tablist"
          aria-orientation="vertical"
          onKeyDown={onTablistKeyDown}
          className="settings-nav flex w-40 shrink-0 flex-col border-r border-line bg-surface p-2"
        >
          {visibleTabs.map((t) => (
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
      )}
      {/*
        手机端要靠 `order` 把标题栏排到 Tab 条之前（DOM 里 nav 是 shell 的第一个
        子元素，标题栏却嵌在这个包裹层里）；跨层级无法直接重排，故手机端给这一层
        `display: contents` 让它「消失」，其子元素升为 shell 的 flex item。桌面端
        该类无任何样式命中，DOM 与观感逐字节不变。
      */}
      <div className="settings-body flex min-w-0 flex-1 flex-col">
        <div className="settings-header flex items-center justify-between border-b border-line px-4 py-2">
          <div className="flex items-center gap-2">
            {/* 手机端子页的返回：44px 触控目标，与关闭按钮同一标准。 */}
            {isMobile && phonePane === "panel" && (
              <button
                ref={backRef}
                type="button"
                aria-label="返回设置分类"
                onClick={() => {
                  // 记下来源分类，等列表挂载后由上面的 effect 落焦。
                  setPendingFocus(active);
                  setPhonePane("list");
                }}
                className="flex size-11 shrink-0 items-center justify-center rounded text-fg-subtle hover:text-fg"
              >
                ‹
              </button>
            )}
            <span className="text-xs text-fg-subtle">设置</span>
          </div>
          {/* 手机端给足 44px 触控目标（Apple HIG，与抽屉按钮同一标准）——手机上
              遮罩被 CSS 隐藏，关闭只能靠这颗按钮或 Escape。桌面沿用原样（`px-2`），
              保持「桌面逐字节不变」这条硬不变量。 */}
          <button
            type="button"
            aria-label="关闭设置"
            onClick={onClose}
            className={`${
              isMobile ? "flex size-11 shrink-0 items-center justify-center" : "px-2"
            } rounded text-fg-subtle hover:text-fg`}
          >
            ×
          </button>
        </div>
        {isMobile && phonePane === "list" ? (
          // 分类列表：每行一个分类，点进去才渲染面板。
          <nav className="settings-categories flex min-h-0 flex-1 flex-col">
            {visibleTabs.map((t) => (
              <button
                key={t.id}
                ref={(el) => {
                  categoryRefs.current[t.id] = el;
                }}
                type="button"
                data-settings-category={t.id}
                onClick={() => {
                  setActive(t.id);
                  setPhonePane("panel");
                }}
                className="flex items-center justify-between border-b border-line px-4 text-left text-sm text-fg"
              >
                {t.label}
                <span aria-hidden="true" className="text-fg-subtle">
                  ›
                </span>
              </button>
            ))}
          </nav>
        ) : (
          // 一次只渲染选中的面板；id/aria-labelledby 在桌面端与 tablist 成对，
          // 手机端没有 tablist，故那对属性不再有匹配目标（无害，保留以免两端分叉）。
          <div
            role="tabpanel"
            id={panelId(active)}
            aria-labelledby={isMobile ? undefined : tabId(active)}
            className="settings-panel min-h-0 flex-1 overflow-y-auto"
          >
            {active === "general" ? (
              <SettingsGeneralTab
                theme={theme}
                onThemeChange={onThemeChange}
                watchmanEnabled={watchmanEnabled}
                onToggleWatchman={onToggleWatchman}
                watchmanWarning={watchmanWarning}
              />
            ) : active === "remote" ? (
              <SettingsRemoteTab
                call={remoteCall}
                initialRelayUrl={relayUrl}
                saveRelayUrl={saveRelayUrl}
              />
            ) : active === "plugins" ? (
              <SettingsPluginsTab
                call={pluginCall}
                projects={pluginProjects ?? []}
                defaultProject={pluginDefaultProject}
              />
            ) : active === "models" ? (
              <SettingsModelsTab call={modelCall} isMobile={isMobile} />
            ) : active === "skills" ? (
              <SettingsSkillsTab
                call={skillsCall}
                projects={skillsProjects ?? []}
                defaultProject={skillsDefaultProject}
              />
            ) : active === "mcp" ? (
              <SettingsMcpTab
                call={mcpCall}
                projects={mcpProjects ?? []}
                defaultProject={mcpDefaultProject}
              />
            ) : active === "tags" ? (
              <SettingsTagsTab
                tagNames={tagNames ?? DEFAULT_TAG_NAMES}
                onSave={saveTagName ?? (() => {})}
              />
            ) : active === "flags" && flagsCall ? (
              <SettingsFlagsTab call={flagsCall} />
            ) : null}
          </div>
        )}
      </div>
    </Modal>
  );
}
