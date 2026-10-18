/**
 * 设置页各 Tab 共用的「在配置哪个项目」选择器。
 *
 * 插件清单与 skill 的三层根都按项目解析（`<项目>/.yi-agent/...`），而桌面侧车
 * app-server 的 workdir 是用户 home，所以每个这类面板都必须让用户明确选项目，
 * 并把它随每次 RPC 传下去——少了这一步，看板跑着、设置页却是空的。
 *
 * `ariaLabel` 由调用方给：两个 Tab 的可访问名不同，测试也按各自的名字查询。
 */
export function SettingsProjectPicker({
  options,
  selected,
  onSelect,
  ariaLabel,
}: {
  /** 候选项目（绝对路径）。空表时不渲染——没有可选项时给个空下拉框只会误导。 */
  options: string[];
  selected: string;
  onSelect: (project: string) => void;
  ariaLabel: string;
}) {
  if (options.length === 0) return null;
  return (
    <label className="flex items-center gap-3 border-b border-line p-4 text-sm">
      <span>项目</span>
      <select
        aria-label={ariaLabel}
        className="min-w-0 flex-1 rounded border border-line bg-surface px-2 py-1 font-mono text-xs"
        value={selected}
        onChange={(e) => onSelect(e.target.value)}
      >
        {options.map((project) => (
          <option key={project} value={project}>
            {project}
          </option>
        ))}
      </select>
    </label>
  );
}
