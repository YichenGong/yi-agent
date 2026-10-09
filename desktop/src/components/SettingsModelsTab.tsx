import { useEffect, useState } from "react";
import {
  deleteModel,
  importEnvModel,
  isModelNotFound,
  listModels,
  setDefaultModel,
  setSubagentModel,
  upsertModel,
  type EffectiveModel,
  type ModelEntryView,
  type ModelList,
  type ModelRpc,
  type ModelUpsertInput,
} from "../lib/models";

/**
 * 宿主注入的模型 RPC 接缝。
 *
 * 非泛型签名（与 SettingsDialog 的 `modelCall` 同形）：宿主传进来的与测试里手写的
 * 普通函数都能直接赋值；运行时形状与 `ModelRpc` 一致，组件内一处断言补回泛型。
 */
export type ModelsCall = (method: string, params: unknown) => Promise<unknown>;

/** 编辑中的草稿。`original === null` 表示新增，否则是「编辑旧名 original」。 */
type Draft = {
  original: string | null;
  name: string;
  provider: "anthropic" | "openai";
  api_url: string;
  model: string;
  api_key: string;
  /** 用户是否真的动过 key 框——没动就绝不发送 `api_key`（= 保留宿主里的旧 key）。 */
  keyEdited: boolean;
};

const EMPTY: ModelList = {
  models: [],
  default_model: null,
  subagent_model: null,
  effective: null,
};

/** 「跟随全局默认」哨兵值：模型名不可能是空串（宿主拒绝空名），故 `""` 安全。 */
const FOLLOW_GLOBAL = "";

/** 把写失败翻成一句给人看的话；「模型不存在」单列，因为它对应的是「被删了」。 */
function saveErrorText(error: unknown): string {
  if (isModelNotFound(error)) return "模型不存在，可能已被删除";
  const message = (error as { message?: unknown } | null)?.message;
  if (typeof message === "string" && message.length > 0) return message;
  return error instanceof Error ? error.message : String(error);
}

/**
 * 生效来源的密钥指示。
 *
 * 只说「有没有」和掩码，原文从不经过桌面端——状态行是给人看的诚实结论，不是凭证面板。
 */
function effectiveKeyLabel(effective: EffectiveModel): string {
  return effective.has_key ? `密钥 ${effective.api_key_masked}` : "未设置密钥";
}

/**
 * 桌面「模型」设置页。
 *
 * 机器级模型清单（`~/.yi-agent/models.json`）的增删改，外加两个全局引用
 * （默认模型 / 子代理模型）。所有 RPC 都走注入的 `call` 接缝，组件不碰全局。
 *
 * 两条硬规则：
 * - **写成功后重读**（无乐观更新）：以宿主回包的清单为准，本地草稿一律丢弃；
 *   失败则内联报错并**保留用户输入**，让他改一改再存。
 * - **key 默认不动**：key 框不回填（掩码也不回填），只有用户真的改过才发送
 *   `api_key`；留空即「保留旧 key」。
 */
export function SettingsModelsTab({ call }: { call?: ModelsCall }) {
  // 接缝在运行时就是 ModelRpc 的形状；一次性补回调用方自选返回类型的泛型能力。
  const rpc = call as ModelRpc | undefined;

  const [catalog, setCatalog] = useState<ModelList>(EMPTY);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [busy, setBusy] = useState(false);

  // 接缝在运行时不变（宿主用稳定函数、测试用同一个 vi.fn），但 effect 只认它的
  // 身份：换身份才重读，避免用户没保存的草稿被无关重渲染冲掉。
  useEffect(() => {
    if (!rpc) {
      setCatalog(EMPTY);
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    void (async () => {
      try {
        const next = await listModels(rpc);
        if (!cancelled) {
          setCatalog(next);
          setLoadError(null);
        }
      } catch {
        if (!cancelled) setLoadError("无法读取模型清单");
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [rpc]);

  /** 写成功后重读：不接受本地草稿当事实，一切以宿主回包为准。 */
  const reload = async () => {
    if (!rpc) return;
    try {
      setCatalog(await listModels(rpc));
      setLoadError(null);
    } catch {
      setLoadError("无法读取模型清单");
    }
  };

  const patch = (fields: Partial<Draft>) =>
    setDraft((current) => (current ? { ...current, ...fields } : current));

  const startAdd = () => {
    setActionError(null);
    setDraft({
      original: null,
      name: "",
      provider: "anthropic",
      api_url: "",
      model: "",
      api_key: "",
      keyEdited: false,
    });
  };

  const startEdit = (entry: ModelEntryView) => {
    setActionError(null);
    // key 框留空：不回填掩码、更不回填原文。
    setDraft({
      original: entry.name,
      name: entry.name,
      provider: entry.provider,
      api_url: entry.api_url,
      model: entry.model,
      api_key: "",
      keyEdited: false,
    });
  };

  const saveDraft = async () => {
    if (!rpc || !draft) return;
    setBusy(true);
    setActionError(null);
    try {
      const input: ModelUpsertInput = {
        name: draft.name,
        provider: draft.provider,
        api_url: draft.api_url,
        model: draft.model,
      };
      // 只有用户真的改过 key 才发送：省略即保留。
      if (draft.keyEdited) input.api_key = draft.api_key;
      await upsertModel(rpc, input);
      setDraft(null);
      await reload();
    } catch (error) {
      // 保留草稿：用户改的值还在，重试或修正后再存。
      setActionError(saveErrorText(error));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (name: string) => {
    if (!rpc) return;
    setBusy(true);
    setActionError(null);
    try {
      await deleteModel(rpc, name);
      await reload();
    } catch (error) {
      setActionError(saveErrorText(error));
    } finally {
      setBusy(false);
    }
  };

  const changeDefault = async (value: string) => {
    if (!rpc) return;
    setBusy(true);
    setActionError(null);
    try {
      await setDefaultModel(rpc, value === FOLLOW_GLOBAL ? null : value);
      await reload();
    } catch (error) {
      setActionError(saveErrorText(error));
    } finally {
      setBusy(false);
    }
  };

  const changeSubagent = async (value: string) => {
    if (!rpc) return;
    setBusy(true);
    setActionError(null);
    try {
      await setSubagentModel(rpc, value === FOLLOW_GLOBAL ? null : value);
      await reload();
    } catch (error) {
      setActionError(saveErrorText(error));
    } finally {
      setBusy(false);
    }
  };

  /**
   * 收边：把 `.env` 当前配置导入清单并设为默认。
   *
   * 同样遵守「写后重读」：导入后 `effective.source` 会从 `env` 变 `catalog`，
   * 状态行随宿主回包自己收口，本地不做乐观改判。
   */
  const importCurrent = async () => {
    if (!rpc) return;
    setBusy(true);
    setActionError(null);
    try {
      await importEnvModel(rpc);
      await reload();
    } catch (error) {
      setActionError(saveErrorText(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="p-5">
      <h2 className="text-sm font-medium text-fg">模型</h2>
      <p className="mt-2 text-xs text-fg-subtle">
        这里配置的是这台机器上的模型清单（全局生效）。全局默认模型用于普通对话，
        子 agent 模型用于后台 worker；「跟随全局默认」表示不单独指定。
      </p>

      {actionError !== null && (
        <p role="alert" className="mt-3 text-xs text-red-400">
          {actionError}
        </p>
      )}

      {!rpc ? (
        <p className="mt-3 text-xs text-fg-subtle">未连接到桌面服务</p>
      ) : loading ? (
        <p className="mt-3 text-xs text-fg-subtle">正在读取模型…</p>
      ) : loadError !== null ? (
        <p role="alert" className="mt-3 text-xs text-red-400">
          {loadError}
        </p>
      ) : catalog.models.length === 0 ? (
        <p className="mt-3 text-sm text-fg-muted">还没有配置任何模型</p>
      ) : (
        <table className="mt-3 w-full table-auto text-left text-sm">
          <thead>
            <tr className="text-xs text-fg-subtle">
              <th className="py-1 pr-2 font-normal">名称</th>
              <th className="py-1 pr-2 font-normal">提供方</th>
              <th className="py-1 pr-2 font-normal">模型标识</th>
              <th className="py-1 pr-2 font-normal">API 地址</th>
              <th className="py-1 pr-2 font-normal">密钥</th>
              <th className="py-1 font-normal">操作</th>
            </tr>
          </thead>
          <tbody>
            {catalog.models.map((entry) => (
              <tr key={entry.name} className="border-t border-line">
                <td className="py-1.5 pr-2 text-fg">{entry.name}</td>
                <td className="py-1.5 pr-2 text-fg-muted">{entry.provider}</td>
                <td className="py-1.5 pr-2 text-fg-muted">{entry.model}</td>
                <td className="py-1.5 pr-2 font-mono text-xs text-fg-subtle">{entry.api_url}</td>
                {/* key 只以掩码示人：原始值从不经过桌面端。 */}
                <td className="py-1.5 pr-2 font-mono text-xs text-fg-subtle">
                  {entry.has_key ? entry.api_key_masked : "未设置"}
                </td>
                <td className="py-1.5">
                  <button
                    type="button"
                    onClick={() => startEdit(entry)}
                    className="rounded px-2 py-0.5 text-xs text-fg-muted hover:text-fg"
                  >
                    编辑
                  </button>
                  <button
                    type="button"
                    aria-label={`删除 ${entry.name}`}
                    onClick={() => void remove(entry.name)}
                    className="ml-1 rounded px-2 py-0.5 text-xs text-fg-muted hover:text-fg"
                  >
                    删除
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {/* 生效来源：无论清单是否为空都要如实告知现在到底在用哪个模型。 */}
      {catalog.effective !== null && (
        <p className="mt-4 text-xs text-fg-subtle">
          {catalog.effective.source === "catalog" ? (
            <>
              当前生效：{catalog.effective.model}（清单条目「{catalog.effective.model_ref}」）
            </>
          ) : (
            <>
              当前实际在用 {catalog.effective.model}（来自 .env，尚未纳入清单）
            </>
          )}
          {" · "}
          {catalog.effective.provider}
          {" · "}
          <span className="font-mono">{catalog.effective.api_url}</span>
          {" · "}
          {effectiveKeyLabel(catalog.effective)}
          {catalog.effective.source === "env" && (
            <button
              type="button"
              onClick={() => void importCurrent()}
              disabled={!rpc || busy}
              className="ml-2 rounded border border-line-strong px-2 py-0.5 text-xs text-fg-muted hover:text-fg disabled:opacity-50"
            >
              导入当前配置
            </button>
          )}
        </p>
      )}

      <div className="mt-4 flex flex-wrap gap-4">
        <label className="flex items-center gap-2 text-xs text-fg-muted">
          <span>全局默认模型</span>
          <select
            value={catalog.default_model ?? FOLLOW_GLOBAL}
            onChange={(e) => void changeDefault(e.target.value)}
            disabled={!rpc}
            className="rounded border border-line bg-surface px-2 py-1 text-xs"
          >
            <option value={FOLLOW_GLOBAL}>跟随全局默认</option>
            {catalog.models.map((entry) => (
              <option key={entry.name} value={entry.name}>
                {entry.name}
              </option>
            ))}
          </select>
        </label>

        <label className="flex items-center gap-2 text-xs text-fg-muted">
          <span>子 agent 模型</span>
          <select
            value={catalog.subagent_model ?? FOLLOW_GLOBAL}
            onChange={(e) => void changeSubagent(e.target.value)}
            disabled={!rpc}
            className="rounded border border-line bg-surface px-2 py-1 text-xs"
          >
            <option value={FOLLOW_GLOBAL}>跟随全局默认</option>
            {catalog.models.map((entry) => (
              <option key={entry.name} value={entry.name}>
                {entry.name}
              </option>
            ))}
          </select>
        </label>
      </div>

      {draft === null ? (
        <button
          type="button"
          onClick={startAdd}
          disabled={!rpc}
          className="mt-4 rounded-md border border-line-strong px-3 py-1.5 text-sm text-fg-muted hover:text-fg disabled:opacity-50"
        >
          添加模型
        </button>
      ) : (
        <div className="mt-4 rounded-md border border-line p-3">
          <h3 className="text-xs font-medium text-fg">
            {draft.original === null ? "新增模型" : `编辑 ${draft.original}`}
          </h3>
          <div className="mt-3 grid grid-cols-2 gap-3">
            <label className="flex flex-col gap-1 text-xs text-fg-muted">
              名称
              <input
                value={draft.name}
                onChange={(e) => patch({ name: e.target.value })}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              />
            </label>
            <label className="flex flex-col gap-1 text-xs text-fg-muted">
              提供方
              <select
                value={draft.provider}
                onChange={(e) => patch({ provider: e.target.value as Draft["provider"] })}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              >
                <option value="anthropic">anthropic</option>
                <option value="openai">openai</option>
              </select>
            </label>
            <label className="flex flex-col gap-1 text-xs text-fg-muted">
              API 地址
              <input
                value={draft.api_url}
                onChange={(e) => patch({ api_url: e.target.value })}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              />
            </label>
            <label className="flex flex-col gap-1 text-xs text-fg-muted">
              模型标识
              <input
                value={draft.model}
                onChange={(e) => patch({ model: e.target.value })}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              />
            </label>
            <label className="flex flex-col gap-1 text-xs text-fg-muted">
              API 密钥
              <input
                type="password"
                value={draft.api_key}
                placeholder={draft.original === null ? "" : "留空则保留原密钥"}
                onChange={(e) => patch({ api_key: e.target.value, keyEdited: true })}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              />
            </label>
          </div>
          <div className="mt-3 flex items-center gap-2">
            <button
              type="button"
              onClick={() => void saveDraft()}
              disabled={busy || !rpc}
              className="rounded-md border border-line-strong px-3 py-1.5 text-sm text-fg-muted hover:text-fg disabled:opacity-50"
            >
              保存
            </button>
            <button
              type="button"
              onClick={() => {
                setDraft(null);
                setActionError(null);
              }}
              className="rounded-md px-3 py-1.5 text-sm text-fg-subtle hover:text-fg"
            >
              取消
            </button>
          </div>
        </div>
      )}
    </section>
  );
}
