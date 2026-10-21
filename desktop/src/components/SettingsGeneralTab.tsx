import type { Theme } from "../lib/theme";

const CHOICES: Array<{ value: Theme; label: string }> = [
  { value: "dark", label: "深色" },
  { value: "light", label: "浅色" },
];

/**
 * 「通用」页：主题，以及宿主级「后台值守」。
 *
 * 值守原来放在看板面板里，是个错位：它装的是 `ai.yi-agent.resident-daemon-watchman`，
 * 服务所有登记在册的项目，与用户当时选中的那个看板毫无关系。放到这里之后，
 * 看板面板也就不必再问一次「要不要在后台跑」——那是宿主的属性。
 */
export function SettingsGeneralTab({
  theme,
  onThemeChange,
  watchmanEnabled,
  onToggleWatchman,
  watchmanWarning,
}: {
  theme: Theme;
  onThemeChange: (theme: Theme) => void;
  /**
   * 宿主级值守开关的当前值。缺省 `undefined` 表示这个宿主没提供该能力
   * （例如只读客户端），此时不渲染这一节，而不是显示一个拨不动的开关。
   */
  watchmanEnabled?: boolean;
  onToggleWatchman?: (next: boolean) => void;
  /** 宿主安装/卸载登录项失败时的说明；空即无异常。 */
  watchmanWarning?: string | null;
}) {
  const showWatchman = watchmanEnabled !== undefined && onToggleWatchman !== undefined;
  return (
    <section className="p-5">
      <h2 className="text-sm font-medium text-fg">主题</h2>
      <div className="mt-3 inline-flex rounded-md border border-line overflow-hidden">
        {CHOICES.map((c) => {
          const active = c.value === theme;
          return (
            <button
              key={c.value}
              type="button"
              aria-pressed={active}
              onClick={() => onThemeChange(c.value)}
              className={`px-4 py-1.5 text-sm ${
                active ? "bg-raised text-fg" : "text-fg-muted hover:bg-raised/50"
              }`}
            >
              {c.label}
            </button>
          );
        })}
      </div>
      <p className="mt-4 text-xs text-fg-subtle">
        也可以直接对话让 Yi-Agent 切换主题，例如「帮我切成浅色」。
      </p>
      {showWatchman && (
        <div className="mt-8 border-t border-line pt-6">
          <label className="flex items-center gap-3 text-sm">
            <input
              type="checkbox"
              aria-label="后台值守（开机自启）开关"
              checked={watchmanEnabled}
              onChange={(event) => onToggleWatchman(event.target.checked)}
            />
            <span>后台值守（开机自启）</span>
          </label>
          <p className="mt-1 text-xs text-fg-subtle">
            开启后常驻 daemon 会在后台持续运行，电脑重启后自动恢复，无需打开本应用。
            默认开启，服务所有已登记的项目。
          </p>
          {watchmanWarning && (
            <p role="alert" className="mt-1 text-xs text-amber-500">
              {watchmanWarning}
            </p>
          )}
        </div>
      )}
    </section>
  );
}
