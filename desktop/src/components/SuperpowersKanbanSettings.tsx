import type { SwitchSource } from "../lib/superpowersKanbanSwitch";

export function SuperpowersKanbanSettings({
  switchOn,
  source,
  onToggle,
  watchmanEnabled,
  onToggleWatchman,
  watchmanWarning,
}: {
  switchOn: boolean;
  source: SwitchSource;
  onToggle: (next: boolean) => void;
  /**
   * 宿主级「后台值守」。默认开，且属于宿主本身而非某个项目，因此与上面的
   * 项目开关分开一行：两者管的不是同一件事。
   */
  watchmanEnabled: boolean;
  onToggleWatchman: (next: boolean) => void;
  /** 宿主安装/卸载值守失败时的说明；空即无异常。 */
  watchmanWarning?: string | null;
}) {
  return (
    <section className="p-4">
      <div className="flex items-center justify-between">
        <label className="flex items-center gap-3 text-sm">
          <input
            type="checkbox"
            aria-label="Superpowers 看板开关"
            checked={switchOn}
            onChange={(event) => onToggle(event.target.checked)}
          />
          <span>Superpowers 看板</span>
          <span className="text-fg-subtle">({source})</span>
        </label>
      </div>
      <p className="mt-2 text-xs text-fg-subtle">
        Off by default. Turning it off stops the plugin from advancing the queue; it
        does not cancel sessions already running in the daemon.
      </p>
      <label className="mt-3 flex items-center gap-3 text-sm">
        <input
          type="checkbox"
          aria-label="后台值守（开机自启）开关"
          checked={watchmanEnabled}
          onChange={(event) => onToggleWatchman(event.target.checked)}
        />
        <span>后台值守（开机自启）</span>
      </label>
      <p className="mt-1 text-xs text-fg-subtle">
        开启后看板将在后台持续运行，电脑重启后自动恢复，无需打开本应用。
      </p>
      {watchmanWarning && (
        <p role="alert" className="mt-1 text-xs text-amber-500">
          {watchmanWarning}
        </p>
      )}
    </section>
  );
}
