import { useEffect, useState, type ReactNode } from "react";
import {
  deleteModel,
  importEnvModel,
  isModelNotFound,
  listModels,
  looksMissingV1,
  setDefaultModel,
  setSubagentModel,
  probeModel,
  testModel,
  upsertModel,
  type EffectiveModel,
  type ModelEntryView,
  type ModelList,
  type ModelRpc,
  type ModelUpsertInput,
} from "../lib/models";
import { REASONING_EFFORTS, supportedEffortsOf } from "../lib/modelPicker";
import { onModelCapabilitiesChanged } from "../lib/modelCapabilitiesBus";

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
  /**
   * 「高级」区里的 `api_style`：`""` = 未选择（不发送，保留宿主既有声明）。
   * 普通用户无需展开；`auto` 与「不发送」等价（服务端解析期默认就是 auto）。
   */
  api_style: string;
  /**
   * 推理档位：`""` = 未设置（不发送，保留宿主既有声明），否则七档之一。
   * 与「API 格式」平级的正常字段。
   */
  reasoning_effort: string;
};

/**
 * 桌面表格的列宽。
 *
 * 必须配 `table-fixed`：auto 布局下列宽由内容 max-content 决定，一条长模型名
 * 或长 URL 会把表格撑到 822px（真实 Chrome 实测，容器只有 590px），横向溢出
 * 而面板不可横向滚动 = 右侧内容永久不可达。固定布局 + 百分比让长内容按列宽
 * 截断（`truncate`），窗口多窄都不溢。
 *
 * 「操作」列给的是**内容宽**而非百分比：三个按钮必须并排一行，它不该参与
 * 比例分配。
 */
const COLUMN_WIDTH: Record<string, string> = {
  模型: "w-[30%]",
  连接: "w-[38%]",
  推理档位: "w-[20%]",
};

/** 「未设置」哨兵值：空串不可能是合法档位 / 通道，故安全。 */
const UNSET = "";

const EMPTY: ModelList = {
  models: [],
  default_model: null,
  subagent_model: null,
  effective: null,
};

/** 「跟随全局默认」哨兵值：模型名不可能是空串（宿主拒绝空名），故 `""` 安全。 */
const FOLLOW_GLOBAL = "";

/**
 * openai 兼容网关的地址通常以 `/v1` 结尾。这里只做启发式提醒，不下硬结论：
 * 含 `/v1` 前缀（如 `/openai/v1`）即视为无虞，无法判断的（如 `/api`）宁可少提示。
 */
function suspectsMissingV1(entry: ModelEntryView): boolean {
  // 清单条目要求 api_url 非空（`model/upsert` 拒绝空串），所以这里的空白
  // 分支在生产中不可达；保留它只是让共享函数对两种入口都安全。
  return looksMissingV1(entry.provider, entry.api_url);
}

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
 * 清单行/卡片上的能力摘要：档位胶囊，或一句「未知/不支持」。
 *
 * 只显示**生效通道**上的结论（与下拉、运行期同源），不把三条通道都摊开：用户
 * 关心的是「这个模型现在能选哪几档」，而不是探测实现细节。
 *
 * 胶囊而非 `low · medium · high` 一行文本：档位名之间没有天然断点，长一点的
 * 集合（如 `none · minimal · low · medium · high · xhigh · max`）会被表格挤到
 * 折成三行，把整行高度顶起来。胶囊各自 `nowrap`、整体 `flex-wrap`，窄了只在
 * **胶囊之间**换行，不会把一个词拆开。
 */
function EffortSummary({ entry }: { entry: ModelEntryView }) {
  const supported = supportedEffortsOf(entry);
  if (supported === null) {
    // 有记录但出错 → 同样是「未知」，但要让人知道试过了。
    const caps = entry.reasoning_capabilities;
    const attempted = caps
      ? (["chat", "responses", "anthropic"] as const).some((ch) => caps[ch] != null)
      : false;
    return (
      <span className="text-xs text-fg-subtle">
        {attempted ? "未探测成功" : "未探测"}
      </span>
    );
  }
  if (supported.length === 0) {
    return <span className="text-xs text-fg-subtle">不支持显式档位</span>;
  }
  // 最多三颗：档位全集有 7 个，全摆出来在设置页那一列要占四行，行高被撑到
  // 名称那一列的两倍。其余折成 `+N`，悬停（title）能看到完整清单。
  const visible = supported.slice(0, 3);
  const hidden = supported.length - visible.length;
  return (
    <span className="flex flex-wrap gap-1" title={`支持：${supported.join(" · ")}`}>
      {visible.map((tier) => (
        <span
          key={tier}
          className="whitespace-nowrap rounded border border-line bg-raised px-1 py-0.5 font-mono text-[0.6875rem] leading-tight text-fg-muted"
        >
          {tier}
        </span>
      ))}
      {hidden > 0 && (
        <span className="whitespace-nowrap px-1 py-0.5 font-mono text-[0.6875rem] leading-tight text-fg-subtle">
          +{hidden}
        </span>
      )}
    </span>
  );
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
export function SettingsModelsTab({
  call,
  isMobile = false,
}: {
  call?: ModelsCall;
  /** 手机端（窄屏）：5 列表格换成纵向卡片，见下方 `catalogFields`。 */
  isMobile?: boolean;
}) {
  // 接缝在运行时就是 ModelRpc 的形状；一次性补回调用方自选返回类型的泛型能力。
  const rpc = call as ModelRpc | undefined;

  const [catalog, setCatalog] = useState<ModelList>(EMPTY);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [busy, setBusy] = useState(false);
  /** 最近一次「测试」的结论：null = 还没测；测成功显示成功，失败显示原因。 */
  const [testResult, setTestResult] = useState<
    { ok: boolean; reason: string | null } | null
  >(null);
  const [testing, setTesting] = useState(false);
  /** 「▸ 高级」折叠区：默认收起，普通用户无需展开。 */
  const [advancedOpen, setAdvancedOpen] = useState(false);
  /**
   * 正在探测的条目名。宿主只回「已启动」，结果经全局通知异步到达；这份本地集合
   * 让按钮能显示「探测中…」，并在收到该条目的通知（或兜底超时）后自行清空。
   */
  const [probing, setProbing] = useState<ReadonlySet<string>>(() => new Set());

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

  // 探测在后台跑完时会推 `model/capabilitiesChanged`。收到后重读清单（能力权威
  // 值只在清单里），并清掉「探测中」标记——否则按钮会永远停在那三个字上。
  useEffect(() => {
    if (!rpc) return;
    let cancelled = false;
    const off = onModelCapabilitiesChanged((change) => {
      setProbing((current) => {
        if (!current.has(change.name)) return current;
        const next = new Set(current);
        next.delete(change.name);
        return next;
      });
      void (async () => {
        try {
          const next = await listModels(rpc);
          if (!cancelled) {
            setCatalog(next);
            setLoadError(null);
          }
        } catch {
          // 读失败保留现状：下一次手动操作还会重读。
        }
      })();
    });
    return () => {
      cancelled = true;
      off();
    };
  }, [rpc]);

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
      api_style: UNSET,
      reasoning_effort: UNSET,
    });
    setAdvancedOpen(false);
    setTestResult(null);
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
      api_style: entry.api_style ?? UNSET,
      reasoning_effort: entry.reasoning_effort ?? UNSET,
    });
    setAdvancedOpen(false);
    setTestResult(null);
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
      // 通道两项同理：未选择（空串）即不发送 = 保留既有声明，零回归。
      if (draft.api_style !== UNSET) input.api_style = draft.api_style;
      if (draft.reasoning_effort !== UNSET) input.reasoning_effort = draft.reasoning_effort;
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

  const runTest = async () => {
    if (!rpc || !draft) return;
    setTesting(true);
    setTestResult(null);
    setActionError(null);
    try {
      // key 解析约定：改过 key 就传草稿 key，否则传条目名让服务端取已存 key。
      const result = await testModel(rpc, {
        provider: draft.provider,
        model: draft.model,
        api_url: draft.api_url,
        ...(draft.keyEdited
          ? { api_key: draft.api_key }
          : draft.original !== null
            ? { name: draft.original }
            : { api_key: draft.api_key }),
      });
      setTestResult(result);
    } catch (error) {
      setTestResult({ ok: false, reason: saveErrorText(error) });
    } finally {
      setTesting(false);
    }
  };

  /**
   * 重新探测某条模型。只启动；结果经 `model/capabilitiesChanged` 回来。
   *
   * `started:false`（已有同名探测在跑）不报错：用户连点两下时，第一轮的结果
   * 仍会到达并刷新这一行。
   */
  const reprobe = async (name: string) => {
    if (!rpc) return;
    setActionError(null);
    setProbing((current) => new Set(current).add(name));
    try {
      await probeModel(rpc, name);
    } catch (error) {
      setProbing((current) => {
        const next = new Set(current);
        next.delete(name);
        return next;
      });
      setActionError(saveErrorText(error));
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

  /**
   * 草稿可选的档位：新建条目还没有能力数据（`null` → 七档全列），编辑既有条目
   * 时按**草稿里当前填的连接字段**去清单找同名条目 —— 用户刚把 provider 从
   * anthropic 改成 openai，下拉就该跟着换到 openai 那条通道的结论，而不是沿用
   * 打开表单时的旧结果。
   */
  const draftEffortTiers: readonly string[] = (() => {
    if (!draft) return REASONING_EFFORTS;
    const existing =
      draft.original !== null
        ? catalog.models.find((m) => m.name === draft.original)
        : undefined;
    if (!existing) return REASONING_EFFORTS;
    const supported = supportedEffortsOf({
      ...existing,
      provider: draft.provider,
      api_style: draft.api_style === UNSET ? null : draft.api_style,
    });
    return supported ?? REASONING_EFFORTS;
  })();

  /**
   * 清单的一行/一张卡片要展示哪些字段。**只定义一次**，两种形态共用：
   * 表格把它映射成 `<td>`，卡片映射成 `<dt>/<dd>`。字段逻辑（`/v1` 启发式提示、
   * 密钥只显掩码）因此只有一份，不会出现「改了一处漏了另一处」。
   *
   * 分组而不是平铺：能力探测上线后平铺出 7 列，设置弹窗实际内宽约 630px 时
   * 「操作」列只剩 32px——三个按钮被挤成三行、行高 153px（真实 Chrome 实测）。
   * 4 列让每列都有可用宽度，且把天然同属一件事的字段并成一格：
   * 「名称 + 模型标识」= 这一条模型是什么，「格式 + 地址 + 密钥」= 连去哪。
   */
  const catalogFields: Array<{
    label: string;
    /** 该格的主值。 */
    value: (entry: ModelEntryView) => ReactNode;
    /** 桌面表格里主值下方的次要行（更小、更暗）；卡片形态同样渲染。 */
    detail?: (entry: ModelEntryView) => ReactNode;
  }> = [
    {
      label: "模型",
      // 名称与标识都单行截断（`title` 给全文）：不截断时窄窗口下一条长名会把
      // 该格拆成五行，整行行高 179px，而旁边几列只有两三行——一列高、其余空，
      // 比截断更难看，也让「同名不同长」的条目无法对齐阅读。
      value: (entry) => (
        <span className="block w-full min-w-0 max-w-[22rem] truncate" title={entry.name}>
          {entry.name}
        </span>
      ),
      detail: (entry) => (
        <span className="block max-w-[22rem] truncate font-mono text-xs" title={entry.model}>
          {entry.model}
        </span>
      ),
    },
    {
      label: "连接",
      value: (entry) => (
        <>
          <span className="text-xs">{entry.provider}</span>
          <span className="mt-0.5 block max-w-[26rem] truncate font-mono text-xs" title={entry.api_url}>
            {entry.api_url}
          </span>
          {/* /v1 只是启发式提醒，压成一个角标：整句提示会把这一格撑成三行。 */}
          {suspectsMissingV1(entry) && (
            <span
              className="mt-1 inline-block rounded border border-line px-1 py-0.5 text-[0.6875rem] leading-tight text-fg-subtle"
              title="OpenAI 兼容接口地址通常以 /v1 结尾，请确认"
            >
              缺 /v1?
            </span>
          )}
        </>
      ),
      detail: (entry) => (
        // key 只以掩码示人：原始值从不经过桌面端。
        <span className="font-mono text-xs">{entry.has_key ? entry.api_key_masked : "未设置密钥"}</span>
      ),
    },
    {
      label: "推理档位",
      // 只读摘要：完整档位清单在「编辑」表单的下拉里（那里才是选择的地方）。
      // 明确区分「未探测」与「探测过但一档都不支持」——前者该保持宽松，后者是
      // 一个真实的结论。
      value: (entry) => <EffortSummary entry={entry} />,
    },
  ];

  /**
   * 清单的桌面形态：4 列表格。
   *
   * `align-top` 让多行格与单行格从同一基线起排；「操作」列 `whitespace-nowrap`
   * 保证三个按钮永远并排一行——表格被压窄时宁可让别的列换行，也不把按钮拆成
   * 竖排（那是「挤」最直观的来源）。
   */
  const renderCatalogTable = () => (
    <table className="mt-3 w-full table-fixed text-left text-sm">
      <thead>
        <tr className="text-xs text-fg-subtle">
          {catalogFields.map((field) => (
            <th key={field.label} className={`py-1 pr-3 font-normal ${COLUMN_WIDTH[field.label] ?? ""}`}>
              {field.label}
            </th>
          ))}
          <th className="w-[8.5rem] py-1 text-right font-normal">操作</th>
        </tr>
      </thead>
      <tbody>
        {catalog.models.map((entry) => (
          <tr key={entry.name} className="border-t border-line">
            {catalogFields.map((field) => (
              <td key={field.label} className={`py-2 pr-3 align-top ${COLUMN_WIDTH[field.label] ?? ""}`}>
                <div className={field.label === "模型" ? "text-fg" : "text-fg-muted"}>
                  {field.value(entry)}
                </div>
                {field.detail && (
                  <div className="mt-0.5 text-fg-subtle">{field.detail(entry)}</div>
                )}
              </td>
            ))}
            <td className="w-[8.5rem] py-2 align-top">
              {/* 桌面三颗按钮的类名与结构保持原样（`px-2 py-0.5 text-xs`、`ml-1`）：
                  桌面 5 列表格「JSX 与类名不变」。手机卡片另给 44px 触控尺寸——按钮
                  是两种排布各自的呈现细节，不与字段逻辑共享。
                  可见文案缩短（`重新探测` → `重测`）是为了整行不被拆开；可访问名
                  不变（`aria-label` 仍是 `重新探测 <名称>`）。 */}
              <div className="flex items-center justify-end whitespace-nowrap">
                <button
                  type="button"
                  onClick={() => startEdit(entry)}
                  className="rounded px-2 py-0.5 text-xs text-fg-muted hover:text-fg"
                >
                  编辑
                </button>
                <button
                  type="button"
                  aria-label={`重新探测 ${entry.name}`}
                  onClick={() => void reprobe(entry.name)}
                  disabled={probing.has(entry.name)}
                  className="ml-1 rounded px-2 py-0.5 text-xs text-fg-muted hover:text-fg disabled:opacity-50"
                >
                  {probing.has(entry.name) ? "探测中…" : "重测"}
                </button>
                <button
                  type="button"
                  aria-label={`删除 ${entry.name}`}
                  onClick={() => void remove(entry.name)}
                  className="ml-1 rounded px-2 py-0.5 text-xs text-fg-muted hover:text-fg"
                >
                  删除
                </button>
              </div>
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );

  /**
   * 清单的手机形态：一条模型一张纵向卡片。
   *
   * 表格固有约 463px 宽，而 390pt 手机上全屏设置页的内宽约 390px——表格怎么调都
   * 装不下，且面板只允许纵向滚动，被裁掉的列（密钥、操作）永久不可达。卡片把
   * 宽度交给容器，任何宽度下所有字段与按钮都在视野内。字段标签在卡片形态下必须
   * 显式渲染（表格形态下由 `<th>` 承担），这里用 `<dt>` 承担。
   */
  const renderCatalogCards = () => (
    <ul className="mt-3 space-y-3">
      {catalog.models.map((entry) => (
        <li key={entry.name} data-model-card className="rounded-md border border-line p-3">
          <h3 className="text-sm font-medium text-fg">{entry.name}</h3>
          {/* 名称作标题时，模型标识随标题一起呈现（表格里它是「模型」格的次要行）。 */}
          <div className="mt-0.5 break-all font-mono text-xs text-fg-subtle">{entry.model}</div>
          <dl className="mt-2 space-y-2 text-xs">
            {/* 名称已作标题，正文里不再重复。 */}
            {catalogFields
              .filter((field) => field.label !== "模型")
              .map((field) => (
                <div key={field.label}>
                  <dt className="text-fg-subtle">{field.label}</dt>
                  <dd className="break-all text-fg-muted">
                    {field.value(entry)}
                    {field.detail && <div className="mt-0.5 text-fg-subtle">{field.detail(entry)}</div>}
                  </dd>
                </div>
              ))}
          </dl>
          <div className="mt-3 flex items-center gap-2">
            {/* 手机端触控目标：`px-3 py-2` + `text-sm` ≈ 44px 高（Apple HIG）。 */}
            <button
              type="button"
              onClick={() => startEdit(entry)}
              className="rounded border border-line-strong px-3 py-2 text-sm text-fg-muted hover:text-fg"
            >
              编辑
            </button>
            <button
              type="button"
              aria-label={`重新探测 ${entry.name}`}
              onClick={() => void reprobe(entry.name)}
              disabled={probing.has(entry.name)}
              className="rounded border border-line-strong px-3 py-2 text-sm text-fg-muted hover:text-fg disabled:opacity-50"
            >
              {probing.has(entry.name) ? "探测中…" : "重新探测"}
            </button>
            <button
              type="button"
              aria-label={`删除 ${entry.name}`}
              onClick={() => void remove(entry.name)}
              className="rounded border border-line-strong px-3 py-2 text-sm text-fg-muted hover:text-fg"
            >
              删除
            </button>
          </div>
        </li>
      ))}
    </ul>
  );

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
      ) : (
        <>
          {catalog.models.length > 0 && catalog.default_model === null && (
            // 有条目却没有默认：新建会话会直接报「未配置模型」，用户很难自己
            // 关联到"只是没设默认"。`role="status"` 而非 `alert`——它与保存
            // 失败是两件事，不该争用同一个可访问性角色。
            <div role="status" className="mt-3 text-xs text-amber-400">
              还没有设置默认模型：新建会话会提示「未配置模型」。
              <button
                type="button"
                onClick={() => void changeDefault(catalog.models[0].name)}
                className="ml-2 underline"
              >
                把「{catalog.models[0].name}」设为默认
              </button>
            </div>
          )}
          {catalog.models.length === 0 ? (
        // 清单空不等于「没模型可用」：env 兜底在生效时下面那行会说清实际在用哪个，
        // 这里再说「还没有配置任何模型」就成了自相矛盾的假话。
        catalog.effective?.source === "env" ? (
          <p className="mt-3 text-sm text-fg-muted">清单里还没有条目</p>
        ) : (
          <p className="mt-3 text-sm text-fg-muted">还没有配置任何模型</p>
        )
      ) : isMobile ? (
        renderCatalogCards()
      ) : (
        renderCatalogTable()
      )}
        </>
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
              API 格式
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
              推理档位
              <select
                value={draft.reasoning_effort}
                onChange={(e) => patch({ reasoning_effort: e.target.value })}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              >
                <option value={UNSET}>未设置</option>
                {draftEffortTiers.map((tier) => (
                  <option key={tier} value={tier}>
                    {tier}
                  </option>
                ))}
                {/* 已保存的档位后来变得不支持（模型换了/能力重新探测过）：仍然列出
                    并标注，否则 select 会显示成「未设置」，与磁盘上的值不一致。 */}
                {draft.reasoning_effort !== UNSET &&
                  !draftEffortTiers.includes(draft.reasoning_effort) && (
                    <option value={draft.reasoning_effort}>
                      {draft.reasoning_effort}（该模型不支持）
                    </option>
                  )}
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
          {/* 「▸ 高级」：默认收起，仅放 api_style。普通用户无需展开。 */}
          <div className="mt-3">
            <button
              type="button"
              aria-expanded={advancedOpen}
              onClick={() => setAdvancedOpen((v) => !v)}
              className="text-xs text-fg-subtle hover:text-fg-muted"
            >
              {advancedOpen ? "▾" : "▸"} 高级
            </button>
            {advancedOpen && (
              <label className="mt-2 flex flex-col gap-1 text-xs text-fg-muted">
                API 通道
                <select
                  value={draft.api_style}
                  onChange={(e) => patch({ api_style: e.target.value })}
                  className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
                >
                  <option value={UNSET}>自动</option>
                  <option value="responses">responses</option>
                  <option value="chat">chat</option>
                </select>
              </label>
            )}
          </div>
          <div className="mt-3 flex items-center gap-2">
            <button
              type="button"
              onClick={() => void runTest()}
              disabled={testing || !rpc}
              className="rounded-md border border-line-strong px-3 py-1.5 text-sm text-fg-muted hover:text-fg disabled:opacity-50"
            >
              {testing ? "测试中…" : "测试"}
            </button>
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
          {testResult !== null && (
            <p
              className={`mt-2 text-xs ${testResult.ok ? "text-green-400" : "text-red-400"}`}
            >
              {testResult.ok ? "连接成功" : (testResult.reason ?? "连接失败")}
            </p>
          )}
        </div>
      )}
    </section>
  );
}
