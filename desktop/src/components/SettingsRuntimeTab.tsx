import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  parseRuntimeSettings,
  parseRuntimeSettingsWrite,
  type RuntimeField,
  type RuntimeSettingsCall,
  type RuntimeSettingsView,
} from "../lib/runtimeSettings";
import {
  acknowledgeRuntimeSave,
  dirtyKeys,
  editRuntimeDraft,
  emptyDraftState,
  mergeRuntimeRead,
  pendingPatch,
  visibleDrafts,
  type RuntimeDraftState,
} from "../lib/runtimeSettingsDrafts";
import { formatError } from "../lib/errorMessage";

export const GROUP_LABELS: Record<string, string> = {
  agent: "Agent",
  compact: "上下文压缩",
  startup: "启动",
  tui: "终端通知",
  power: "电源与休眠",
};

/** 每组一句话说明在管什么。后端新增分组时查不到就不渲染，不造空话。 */
export const GROUP_HINTS: Record<string, string> = {
  agent: "单次对话的执行上限与提示词覆盖",
  compact: "上下文接近上限时的自动压缩策略",
  tui: "命令行界面在回合结束时的提示方式",
  startup: "启动时生效的默认档位",
  power: "有网且（插电 或 有活在跑）时阻止系统空闲休眠",
};

/**
 * 按「影响面从大到小」排：agent 是主循环，compact 是它的兜底，tui 是另一个前端。
 * `power` 影响整机、改动频率最低，放末尾。
 */
export const GROUP_ORDER = ["agent", "compact", "tui", "startup", "power"];

/**
 * 只在桌面有意义的组。
 *
 * `tui` 整的是**桌面主机的命令行界面**（回合结束提示、终端标题、通知方式）：
 * 配对手机改它，改的是另一台机器上 TUI 的行为，而手机自己永远看不到那些提示。
 * 隐藏只发生在渲染层——字段仍进草稿与 `buildPatch`，保存时不会因此漏项。
 */
export const DESKTOP_ONLY_GROUPS: ReadonlySet<string> = new Set(["tui"]);

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
  "tui.terminal_title": { true: "开", false: "关" },
  "power.prevent_idle_sleep": { true: "开", false: "关" },
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
/**
 * 一般成功文案。
 *
 * 逐字取自 spec §8.3。刻意不说「所有会话均已采用」：保存只表示提交，而显式启动
 * 覆盖、角色安全限制和默认沙箱都可能让某些工作更晚才变。
 */
const SAVED_LIVE_TEXT =
  "已保存；支持热更新的同实例会话将在下一个安全执行点采用，通常无需重启。显式启动覆盖、角色安全限制和默认沙箱的例外见各字段。";

/**
 * 旧宿主只回 `{ok:true}` 时的文案。
 *
 * 只敢说「已保存」：那台宿主不会把改动送到已开始的工作，承诺热更新就是撒谎。
 */
const SAVED_LEGACY_TEXT = "已保存。";

/** 同批里含默认沙箱时的补充说明（spec §8.3）。 */
const SANDBOX_EXCEPTION_TEXT =
  "其中默认沙箱仅影响保存后新建会话；已有会话使用固化基线，旧历史缺少基线时按只读恢复并提示。";

const DEGRADED_TEXT: Record<"last_known_good" | "startup_fallback", string> = {
  last_known_good:
    "配置读取异常，当前使用最后有效值，尚不能保证采用新设置。",
  startup_fallback:
    "配置读取异常，当前使用启动回退值，尚不能保证采用新设置。",
};

export function SettingsRuntimeTab({
  call,
  isMobile = false,
  subscribe,
}: {
  call: RuntimeSettingsCall;
  /**
   * 是否手机端。只影响「显示哪些组」：桌面专属组在手机端不渲染（见
   * {@link DESKTOP_ONLY_GROUPS}）。字段数据本身不受影响，草稿与 patch 仍是全量。
   *
   * 缺省 `false`（桌面），与 `SettingsModelsTab` 同一约定。
   */
  isMobile?: boolean;
  /**
   * 订阅「宿主设置已失效」，用来响应别的窗口/手机/观察器的保存。
   *
   * 由 `App` 经 `SettingsDialog` 传入，因为它需要绑定到**当前 RpcClient**；组件
   * 自己去取全局 client 会在重连后订阅到旧连接上，那台已死的宿主永远不发通知。
   */
  subscribe?: (listener: () => void) => () => void;
}) {
  const [fields, setFields] = useState<RuntimeField[]>([]);
  const [path, setPath] = useState("");
  const [view, setView] = useState<RuntimeSettingsView | null>(null);
  const [draftState, setDraftState] = useState<RuntimeDraftState>(emptyDraftState);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState<{ ok: boolean; text: string } | null>(null);
  /**
   * 本地读请求序号：只有最后发出的那次读可以落地。
   *
   * 慢响应后到会覆盖新值（旧代码里 `setDrafts(draftsOf(...))` 直接抹掉用户输入）。
   * 不能用内容 revision 排序——它是内容标识不是序号，A→B→A 的合法序列里后端可能
   * 出现相同值。
   */
  const latestRead = useRef(0);
  /**
   * 保存期间的失效提示先攒着，保存后只补一次读。
   *
   * 保存本身会触发一次通知；立刻重读会和「保存结束的权威读」抢同一个状态。
   */
  const invalidatedDuringSave = useRef(false);
  /** 保存中标志的镜像，供订阅回调读取，避免把 `saving` 放进 effect 依赖。 */
  const savingRef = useRef(saving);
  savingRef.current = saving;

  /**
   * 读设置。
   *
   * `showSpinner` 只在**首次加载**为真：后台刷新（失效通知、保存后的权威读）必须
   * 原地更新。把表单换回「读取中…」会卸载再重挂输入框——用户在打字时别人保存
   * 一次，光标和选区就丢了，滚动位置也会跳回顶部。
   */
  const load = useCallback(async (showSpinner = false) => {
    const requestId = ++latestRead.current;
    if (showSpinner) setLoading(true);
    try {
      const parsed = parseRuntimeSettings(await call("settings/read", {}));
      // 迟到的旧读：它描述的已经不是当前状态，直接丢弃。
      if (requestId !== latestRead.current) return;
      setFields(parsed.fields);
      setView(parsed);
      // `path` 只在读成功时更新：它是页脚的上下文，读失败时保留上一次的值，
      // 免得底栏闪一下空白。
      setPath(parsed.path);
      // 只合并：未编辑过的字段跟随远端，已编辑的保留。绝不整体覆盖。
      setDraftState((state) => mergeRuntimeRead(state, parsed.fields));
      // 读成功才清掉错误提示；读失败要留着，不能被旧成功提示掩盖。
      setNotice((current) => (current && !current.ok ? null : current));
    } catch (error) {
      if (requestId !== latestRead.current) return;
      // 保留草稿：读失败时抹掉用户输入是最糟的失败模式。
      setNotice({ ok: false, text: formatError(error) });
    } finally {
      if (requestId === latestRead.current) setLoading(false);
    }
  }, [call]);

  useEffect(() => {
    void load(true);
  }, [load]);

  // 别的进程/窗口保存后就重读。通知只表示「失效」，值仍从 settings/read 取。
  //
  // `saving` 走 ref 而不是依赖项：把它放进依赖会让保存开始/结束各退订重订一次，
  // 那两次重订之间到达的通知就永久丢了——而「保存期间别人也保存了」恰恰是最需要
  // 补读的时刻。
  useEffect(() => {
    if (!subscribe) return;
    return subscribe(() => {
      if (savingRef.current) {
        invalidatedDuringSave.current = true;
        return;
      }
      void load();
    });
  }, [subscribe, load]);

  const grouped = useMemo(() => {
    // 手机端先摘掉桌面专属组，再排其余：`extra` 是把不在 `GROUP_ORDER` 里的
    // 组补到末尾，故过滤必须发生在它之前，否则被隐藏的组会经 `extra` 漏回来。
    const visible = isMobile
      ? fields.filter((f) => !DESKTOP_ONLY_GROUPS.has(f.group))
      : fields;
    const order = GROUP_ORDER.filter((group) => visible.some((f) => f.group === group));
    const extra = [...new Set(visible.map((f) => f.group))].filter(
      (group) => !order.includes(group),
    );
    return [...order, ...extra].map((group) => ({
      group,
      fields: visible.filter((f) => f.group === group),
    }));
  }, [fields, isMobile]);

  const save = async (patch: Record<string, string | null>) => {
    if (Object.keys(patch).length === 0) return;
    // 发起保存时快照代数：保存期间若有新编辑，代数会变，那些键不能被清掉。
    //
    // 直接取本次渲染闭包里的状态：`save` 由当前渲染创建，`patch` 也来自同一份
    // 状态，所以这就是「提交那一刻」的代数。
    //
    // **不能**写在 `setDraftState` 的 updater 里顺便取出：updater 何时执行由
    // React 决定（批处理、重渲染时机），把取值当它的副作用就是在依赖内部实现
    // 细节——取到的可能是空对象，于是每个提交键都被判成「代数未变」而被清掉，
    // 保存期间的新输入随之消失。
    const editsAtSubmit: Record<string, number> = { ...draftState.edits };
    setSaving(true);
    setNotice(null);
    try {
      const receipt = parseRuntimeSettingsWrite(await call("settings/write", { patch }));
      setDraftState((state) => acknowledgeRuntimeSave(state, patch, editsAtSubmit));
      setSaving(false);
      // 保存后一次权威读：回执里的 revision 是本次提交版本，之后可能已被别的
      // 进程推进，所以直接问当前值而不是把回执当成新状态。
      invalidatedDuringSave.current = false;
      // 权威读在前、成功文案在后是有意为之，但读失败不能被成功文案盖掉：
      // spec §8.2 要求后续 read 失败必须可见，否则用户看到绿灯却不知道界面上的
      // 值可能已经不是当前值。`load` 失败时它已经把错误写进 notice，这里只在
      // notice 仍然干净（没有新的失败）时才写成功文案。
      await load();
      setNotice((current) =>
        current && !current.ok ? current : { ok: true, text: describeSuccess(receipt, patch) },
      );
    } catch (error) {
      // 服务端把 `invalid_setting` 的原因放在 message 里，直接内联显示。
      // 必须先过 formatError：RPC 失败给的是 `{code, message}` 对象，
      // 直接 String() 会渲染成 "[object Object]"。
      setNotice({ ok: false, text: formatError(error) });
      setSaving(false);
      if (invalidatedDuringSave.current) {
        invalidatedDuringSave.current = false;
        void load();
      }
    }
  };

  /**
   * 成功文案取决于宿主能力与本批内容。
   *
   * 三件事必须同时成立才敢说「热更新」：宿主报了能力版本、这批里有需要热更新的
   * 键、且没有默认沙箱这类明确例外。
   */
  const describeSuccess = (
    receipt: ReturnType<typeof parseRuntimeSettingsWrite>,
    patch: Record<string, string | null>,
  ): string => {
    if (receipt.live_apply_version !== 1) return SAVED_LEGACY_TEXT;
    const touchesSandbox = Object.keys(patch).some((key) => key.startsWith("startup.sandbox"));
    const overridden = receipt.overridden_keys
      .filter((entry) => entry.key in patch)
      .map((entry) => `${entry.key}（被${entry.source === "override" ? "启动覆盖" : "环境变量"}遮蔽）`);
    const parts = [SAVED_LIVE_TEXT];
    if (touchesSandbox) parts.push(SANDBOX_EXCEPTION_TEXT);
    if (overridden.length > 0) parts.push(`未采用：${overridden.join("、")}。`);
    return parts.join(" ");
  };

  /** 「撤销改动」：草稿全部丢掉，显示值回到基线，保存按钮重新禁用。 */
  const resetDrafts = () => {
    setDraftState((state) => ({ ...state, drafts: {}, edits: {}, conflicts: [] }));
    setNotice(null);
  };

  if (loading) return <p className="p-5 text-sm text-fg-subtle">读取中…</p>;

  const values = visibleDrafts(draftState);
  const patch = pendingPatch(draftState);
  const dirty = Object.keys(patch).length > 0;
  const dirtyFieldKeys = new Set(dirtyKeys(draftState));

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

        {view?.health?.state === "degraded" ? (
          <p
            role="alert"
            className="rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-500"
          >
            {DEGRADED_TEXT[view.snapshot_origin === "startup_fallback" ? "startup_fallback" : "last_known_good"]}
            {view.health.code ? ` 错误类别：${view.health.code}。` : ""}
            {view.health.invalid_keys.length > 0
              ? ` 需修复：${view.health.invalid_keys.join("、")}。`
              : ""}
          </p>
        ) : null}

        {draftState.conflicts.length > 0 ? (
          <p
            role="status"
            className="rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-500"
          >
            这些项在别处也被改过，你的草稿已保留：{draftState.conflicts.join("、")}。
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
                          value={values[field.key] ?? ""}
                          onChange={(event) =>
                            setDraftState((state) =>
                              editRuntimeDraft(state, field.key, event.target.value),
                            )
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
                          value={values[field.key] ?? ""}
                          onChange={(event) =>
                            setDraftState((state) =>
                              editRuntimeDraft(state, field.key, event.target.value),
                            )
                          }
                          className={`${CONTROL_CLASS} resize-y`}
                        />
                      ) : (
                        <input
                          id={`runtime-${field.key}`}
                          type={field.type === "number" ? "number" : "text"}
                          min={field.min ?? undefined}
                          max={field.max ?? undefined}
                          value={values[field.key] ?? ""}
                          onChange={(event) =>
                            setDraftState((state) =>
                              editRuntimeDraft(state, field.key, event.target.value),
                            )
                          }
                          className={CONTROL_CLASS}
                        />
                      )}
                      <div className="mt-1 flex items-center justify-between gap-2 text-xs">
                        <span className={isUnset ? "text-fg-subtle" : "text-fg-muted"}>
                          {isUnset
                            ? `默认 ${labelFor(field.key, field.default)}`
                            : `已设为 ${labelFor(field.key, String(field.value))}`}
                          {/* 草稿与已保存值不同：让用户一眼看出哪些还没提交。 */}
                          {dirtyFieldKeys.has(field.key) ? "（未保存）" : ""}
                          {/* 宿主的有效值来自显式来源时逐项说明，否则用户会以为自己
                              在文件里改的值没生效。 */}
                          {field.host_source === "override"
                            ? `　宿主启动覆盖：${String(field.host_value ?? "")}`
                            : field.host_source === "environment"
                              ? `　宿主环境变量：${String(field.host_value ?? "")}`
                              : ""}
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
