import { useCallback, useEffect, useState } from "react";

export type FlagRow = {
  name: string;
  summary: string;
  enabled: boolean;
  expires_on: string;
  source: string;
};

export type FlagsCall = (method: string, params?: unknown) => Promise<unknown>;

/**
 * 「实验功能」Tab：只读注册表 + 逐项开关。
 *
 * 只在 dev 构建里出现（`SettingsDialog` 按 `build/info` 的 `build_kind` 决定是否
 * 渲染这一项）。值不一定来自文件——`source` 会说明它来自默认值、`flags.json`，
 * 还是 `YI_AGENT_FLAG_*` 环境变量；后者是排查「明明开了却没生效」的关键线索。
 */
export function SettingsFlagsTab({ call }: { call: FlagsCall }) {
  const [rows, setRows] = useState<FlagRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const result = (await call("flags/list")) as { flags: FlagRow[] };
      setRows(result.flags);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [call]);

  useEffect(() => {
    void load();
  }, [load]);

  const toggle = async (row: FlagRow, enabled: boolean) => {
    try {
      await call("flags/set", { name: row.name, enabled });
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  if (error) {
    return (
      <p role="alert" className="p-4 text-sm text-red-400">
        读取开关失败：{error}
      </p>
    );
  }
  if (!rows) {
    return <p className="p-4 text-sm text-fg-muted">读取中…</p>;
  }
  if (rows.length === 0) {
    return (
      <p className="p-4 text-sm text-fg-muted">
        还没有注册任何开关。功能做进来时，在 yi-agent-flags 的注册表里登记一项。
      </p>
    );
  }

  return (
    <div className="flex flex-col gap-2 p-4">
      <p className="text-xs text-fg-muted">
        这些是实验功能，只在这个开发版本里可开关；稳定版不存在此页面。
      </p>
      {rows.map((row) => (
        <div
          key={row.name}
          className="flex items-center justify-between gap-3 rounded border border-line p-3"
        >
          <div className="min-w-0">
            <div className="truncate text-sm text-fg">{row.name}</div>
            <div className="truncate text-xs text-fg-muted">{row.summary}</div>
            <div className="text-xs text-fg-subtle">{`来源 ${row.source} · 到期 ${row.expires_on}`}</div>
          </div>
          <input
            type="checkbox"
            aria-label={`${row.name} 开关`}
            checked={row.enabled}
            onChange={(event) => void toggle(row, event.target.checked)}
          />
        </div>
      ))}
    </div>
  );
}
