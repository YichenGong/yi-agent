import { useCallback, useEffect, useMemo, useState } from "react";
import {
  buildPatch,
  parseRuntimeSettings,
  type RuntimeField,
  type RuntimeSettingsCall,
} from "../lib/runtimeSettings";

const GROUP_LABELS: Record<string, string> = {
  agent: "Agent",
  compact: "压缩",
  startup: "启动",
};

const GROUP_ORDER = ["agent", "compact", "startup"];

/**
 * 「运行参数」页。
 *
 * 三态是这一页的核心：`value === null` 是「未设置」，显示默认值并禁用恢复按钮；
 * 有值则显示「已设为 Y」并允许一键恢复。**绝不把 null 归一成默认值**——那正是
 * 旧 web 的 `get_config` 做过的事，它让「保存一次后所有字段都像已设置」。
 *
 * 保存只发用户改过的字段（`buildPatch`），故不会把其余项的当前值整体回写。
 */
export function SettingsRuntimeTab({ call }: { call: RuntimeSettingsCall }) {
  const [fields, setFields] = useState<RuntimeField[]>([]);
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState<{ ok: boolean; text: string } | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const view = parseRuntimeSettings(await call("settings/read", {}));
      setFields(view.fields);
      // 草稿从「当前值」起步，让 buildPatch 能算出用户改了哪一项。
      const next: Record<string, string> = {};
      for (const field of view.fields) {
        next[field.key] = field.value === null ? "" : String(field.value);
      }
      setDrafts(next);
    } catch (error) {
      setNotice({ ok: false, text: String(error) });
    } finally {
      setLoading(false);
    }
  }, [call]);

  useEffect(() => {
    void load();
  }, [load]);

  const grouped = useMemo(() => {
    const order = GROUP_ORDER.filter((group) => fields.some((f) => f.group === group));
    const extra = [...new Set(fields.map((f) => f.group))].filter(
      (group) => !order.includes(group),
    );
    return [...order, ...extra].map((group) => ({
      group,
      fields: fields.filter((f) => f.group === group),
    }));
  }, [fields]);

  const save = async (patch: Record<string, string | null>) => {
    if (Object.keys(patch).length === 0) return;
    setSaving(true);
    setNotice(null);
    try {
      await call("settings/write", { patch });
      await load();
      setNotice({ ok: true, text: "已保存" });
    } catch (error) {
      // 服务端把 `invalid_setting` 的原因放在 message 里，直接内联显示。
      setNotice({ ok: false, text: String(error) });
    } finally {
      setSaving(false);
    }
  };

  if (loading) return <p className="settings-hint">读取中…</p>;

  return (
    <div className="settings-runtime">
      {notice ? (
        <p className={notice.ok ? "settings-hint" : "settings-error"} role="status">
          {notice.text}
        </p>
      ) : null}

      {grouped.map(({ group, fields: rows }) => (
        <section key={group} className="settings-group">
          <h3>{GROUP_LABELS[group] ?? group}</h3>
          {rows.map((field) => {
            const isUnset = field.value === null;
            return (
              <div key={field.key} className="settings-row">
                <label htmlFor={`runtime-${field.key}`}>{field.label}</label>
                {field.type === "select" ? (
                  <select
                    id={`runtime-${field.key}`}
                    value={drafts[field.key] ?? ""}
                    onChange={(event) =>
                      setDrafts((prev) => ({ ...prev, [field.key]: event.target.value }))
                    }
                  >
                    <option value="">未设置（默认 {field.default || "—"}）</option>
                    {field.options.map((option) => (
                      <option key={option} value={option}>
                        {option}
                      </option>
                    ))}
                  </select>
                ) : (
                  <input
                    id={`runtime-${field.key}`}
                    type={field.type === "number" ? "number" : "text"}
                    min={field.min ?? undefined}
                    max={field.max ?? undefined}
                    value={drafts[field.key] ?? ""}
                    placeholder={`未设置 · 默认 ${field.default || "—"}`}
                    onChange={(event) =>
                      setDrafts((prev) => ({ ...prev, [field.key]: event.target.value }))
                    }
                  />
                )}
                <p className="settings-hint">
                  {isUnset ? `未设置 · 默认 ${field.default || "—"}` : `已设为 ${field.value}`}
                </p>
                <p className="settings-hint">{field.description}</p>
                <button
                  type="button"
                  disabled={saving || isUnset}
                  onClick={() => void save({ [field.key]: null })}
                >
                  恢复默认
                </button>
              </div>
            );
          })}
        </section>
      ))}

      <button
        type="button"
        disabled={saving}
        onClick={() => void save(buildPatch(fields, drafts))}
      >
        保存
      </button>
    </div>
  );
}
