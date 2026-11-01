import { useCallback, useEffect, useMemo, useState } from "react";
import {
  buildPatch,
  parseRuntimeSettings,
  type RuntimeField,
  type RuntimeSettingsCall,
} from "../lib/runtimeSettings";
import { formatError } from "../lib/errorMessage";

export const GROUP_LABELS: Record<string, string> = {
  agent: "Agent",
  compact: "上下文压缩",
  startup: "启动",
  tui: "终端通知",
};

/** 每组一句话说明在管什么。后端新增分组时查不到就不渲染，不造空话。 */
export const GROUP_HINTS: Record<string, string> = {
  agent: "单次对话的执行上限与提示词覆盖",
  compact: "上下文接近上限时的自动压缩策略",
  tui: "命令行界面在回合结束时的提示方式",
  startup: "启动时生效的默认档位",
};

/** 按「影响面从大到小」排：agent 是主循环，compact 是它的兜底，tui 是另一个前端。 */
export const GROUP_ORDER = ["agent", "compact", "tui", "startup"];

/**
 * 枚举值的显示文案。**只影响显示**：提交给 `settings/write` 的永远是原始字符串。
 *
 * 不在此表里的字段（数字、自由文本）原样返回——这张表是「补充说明」而不是白名单，
 * 后端将来加选项时前端不至于留空。
 */
const VALUE_LABELS: Record<string, Record<string, string>> = {
  "startup.sandbox": {
    "read-only": "只读",
    "workspace-write": "工作区可写",
    "danger-full-access": "完全访问",
  },
  "tui.notifications": { true: "开", false: "关" },
  "tui.notification_method": { auto: "自动", osc9: "OSC 9（带正文）", bel: "响铃" },
  "tui.notification_condition": { unfocused: "仅窗口失焦时", always: "总是" },
};

/**
 * 控件的外观。三个控件共用一份，避免 select / textarea / input 之间漂移。
 *
 * `bg-raised` + `border-line-strong` 而非 `bg-surface` + `border-line`：本页卡片底色
 * 是 `panel`，#0a0a0a 的 `surface` 落在 #171717 上是一块更黑的「洞」，边框也几乎
 * 看不见；`raised` 比 `panel` 亮一档，控件才读作「凸起的可编辑区」。
 */
const CONTROL_CLASS =
  "w-full rounded border border-line-strong bg-raised px-2.5 py-1.5 text-sm text-fg outline-none focus:border-fg-subtle disabled:opacity-50";

/** 草稿的起点：每个字段取「当前值」，未设置取空串。 */
function draftsOf(fields: RuntimeField[]): Record<string, string> {
  const next: Record<string, string> = {};
  for (const field of fields) {
    next[field.key] = field.value === null ? "" : String(field.value);
  }
  return next;
}

/**
 * 字段值的中文显示。空串显示成破折号——它出现两处：`agent.system_prompt` 的
 * `default_display`（默认就是「不覆盖」），以及用户清空输入框后的草稿值。
 */
export function labelFor(key: string, raw: string): string {
  if (raw === "") return "—";
  return VALUE_LABELS[key]?.[raw] ?? raw;
}

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
  const [path, setPath] = useState("");
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState<{ ok: boolean; text: string } | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const view = parseRuntimeSettings(await call("settings/read", {}));
      setFields(view.fields);
      // `path` 只在读成功时更新：它是页脚的上下文，读失败时保留上一次的值，
      // 免得底栏闪一下空白。
      setPath(view.path);
      // 草稿从「当前值」起步，让 buildPatch 能算出用户改了哪一项。
      setDrafts(draftsOf(view.fields));
    } catch (error) {
      setNotice({ ok: false, text: formatError(error) });
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
      // 必须先过 formatError：RPC 失败给的是 `{code, message}` 对象，
      // 直接 String() 会渲染成 "[object Object]"。
      setNotice({ ok: false, text: formatError(error) });
    } finally {
      setSaving(false);
    }
  };

  /** 「撤销改动」：草稿回到各字段的当前值，`buildPatch` 随之变空、保存按钮重新禁用。 */
  const resetDrafts = () => {
    setDrafts(draftsOf(fields));
    setNotice(null);
  };

  if (loading) return <p className="p-5 text-sm text-fg-subtle">读取中…</p>;

  const patch = buildPatch(fields, drafts);
  const dirty = Object.keys(patch).length > 0;

  if (fields.length === 0) {
    return <p className="p-5 text-sm text-fg-subtle">没有可配置的运行参数</p>;
  }

  return (
    <div className="flex min-h-full flex-col">
      <div className="flex-1 space-y-6 p-5">
        {notice ? (
          <p
            role={notice.ok ? "status" : "alert"}
            className={`rounded-md border px-3 py-2 text-xs ${
              notice.ok
                ? "border-line bg-raised/50 text-fg-muted"
                : "border-amber-500/40 bg-amber-500/10 text-amber-500"
            }`}
          >
            {notice.text}
          </p>
        ) : null}

        {grouped.map(({ group, fields: rows }) => (
          <section key={group}>
            <h3 className="text-sm font-medium text-fg">{GROUP_LABELS[group] ?? group}</h3>
            {GROUP_HINTS[group] ? (
              <p className="mt-1 text-xs text-fg-subtle">{GROUP_HINTS[group]}</p>
            ) : null}
            <div className="@container mt-3 divide-y divide-line rounded-md border border-line">
              {rows.map((field) => {
                const isUnset = field.value === null;
                return (
                  <div
                    key={field.key}
                    className="flex flex-col gap-2 px-3 py-2.5 @lg:flex-row @lg:items-start @lg:justify-between @lg:gap-6"
                  >
                    <div className="min-w-0 @lg:flex-1">
                      <label htmlFor={`runtime-${field.key}`} className="block text-sm text-fg">
                        {field.label}
                      </label>
                      <p className="mt-0.5 text-xs leading-relaxed text-fg-subtle">
                        {field.description}
                      </p>
                    </div>
                    <div className="shrink-0 @lg:w-56">
                      {field.type === "select" ? (
                        <select
                          id={`runtime-${field.key}`}
                          value={drafts[field.key] ?? ""}
                          onChange={(event) =>
                            setDrafts((prev) => ({ ...prev, [field.key]: event.target.value }))
                          }
                          className={CONTROL_CLASS}
                        >
                          <option value="">
                            未设置（默认 {labelFor(field.key, field.default)}）
                          </option>
                          {field.options.map((option) => (
                            <option key={option} value={option}>
                              {labelFor(field.key, option)}
                            </option>
                          ))}
                        </select>
                      ) : field.key === "agent.system_prompt" ? (
                        <textarea
                          id={`runtime-${field.key}`}
                          rows={3}
                          value={drafts[field.key] ?? ""}
                          onChange={(event) =>
                            setDrafts((prev) => ({ ...prev, [field.key]: event.target.value }))
                          }
                          className={`${CONTROL_CLASS} resize-y`}
                        />
                      ) : (
                        <input
                          id={`runtime-${field.key}`}
                          type={field.type === "number" ? "number" : "text"}
                          min={field.min ?? undefined}
                          max={field.max ?? undefined}
                          value={drafts[field.key] ?? ""}
                          onChange={(event) =>
                            setDrafts((prev) => ({ ...prev, [field.key]: event.target.value }))
                          }
                          className={CONTROL_CLASS}
                        />
                      )}
                      <div className="mt-1 flex items-center justify-between gap-2 text-xs">
                        <span className={isUnset ? "text-fg-subtle" : "text-fg-muted"}>
                          {isUnset
                            ? `默认 ${labelFor(field.key, field.default)}`
                            : `已设为 ${labelFor(field.key, String(field.value))}`}
                        </span>
                        {isUnset ? null : (
                          <button
                            type="button"
                            disabled={saving}
                            onClick={() => void save({ [field.key]: null })}
                            className="rounded text-fg-subtle hover:text-fg disabled:opacity-50"
                          >
                            恢复默认
                          </button>
                        )}
                      </div>
                    </div>
                  </div>
                );
              })}
            </div>
          </section>
        ))}
      </div>

      <div className="sticky bottom-0 flex flex-wrap items-center justify-between gap-3 border-t border-line bg-panel px-5 py-3">
        <span className="min-w-0 truncate text-xs text-fg-subtle" title={path}>
          settings.json · {path}
        </span>
        <div className="flex shrink-0 items-center gap-2">
          <button
            type="button"
            disabled={saving || !dirty}
            onClick={() => resetDrafts()}
            className="rounded border border-line-strong px-3 py-1.5 text-sm text-fg-muted disabled:opacity-50"
          >
            撤销改动
          </button>
          <button
            type="button"
            disabled={saving || !dirty}
            onClick={() => void save(patch)}
            className="rounded bg-fg px-3 py-1.5 text-sm text-surface disabled:opacity-50"
          >
            {saving ? "保存中…" : "保存"}
          </button>
        </div>
      </div>
    </div>
  );
}
