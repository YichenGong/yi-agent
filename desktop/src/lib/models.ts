/**
 * `model/*` 与 `thread/setModel` 的桌面端纯封装。
 *
 * 与 `boardIndex.ts` / `pluginSettings.ts` 同一层：不碰 React、不碰具体
 * `RpcClient`，只把「调用意图」翻译成 `method` + `params`，并把宿主错误翻成
 * 一句可判定的谓词。形状取宿主 JSON-RPC 的线格式（`snake_case`），桌面端
 * 其余代码按字段直用，不做二次映射。
 */
export type ModelRpc = <T = unknown>(method: string, params: unknown) => Promise<T>;

/** 模型清单里的一条，与宿主 `model/list` 返回的条目形状一致（key 只回掩码）。 */
export type ModelEntryView = {
  name: string;
  provider: "anthropic" | "openai";
  api_url: string;
  model: string;
  has_key: boolean;
  api_key_masked: string;
};

/**
 * 当前默认**实际解析到哪里**：清单条目（`catalog`）还是回退 `.env`（`env`）。
 *
 * 面板只拿它「如实告知」，不做判断：`source` 是宿主算好的权威结论，前端不复算
 * ——兜底链的细节（环境变量、宿主配置）前端根本看不见。key 只出掩码，原文不经过
 * 桌面端：`has_key` 说「有没有」，`api_key_masked` 是唯一可显示的凭证形态。
 */
export type EffectiveModel = {
  source: "catalog" | "env";
  /** 命中的清单条目名；`source === "env"` 时为 `null`（还没进清单）。 */
  model_ref: string | null;
  provider: string;
  api_url: string;
  model: string;
  has_key: boolean;
  api_key_masked: string;
};

/** `model/list` 的完整回包：清单 + 两个全局引用 + 实际生效的那一份。 */
export type ModelList = {
  models: ModelEntryView[];
  default_model: string | null;
  subagent_model: string | null;
  /** 宿主未提供时回 `null`，面板只渲染已知信息（老宿主/降级都能用）。 */
  effective: EffectiveModel | null;
};

/**
 * 读机器级模型清单。
 *
 * `models` 缺失/非数组时回空表、引用缺失时回 `null`：设置面板只想渲染，
 * 不该因为宿主少发一个字段就整页崩掉。
 */
export async function listModels(rpc: ModelRpc): Promise<ModelList> {
  const result = await rpc<Partial<ModelList>>("model/list", {});
  return {
    models: Array.isArray(result?.models) ? result.models : [],
    default_model: result?.default_model ?? null,
    subagent_model: result?.subagent_model ?? null,
    effective: result?.effective ?? null,
  };
}

/**
 * 把 `.env`（兜底层）当前的模型配置导入清单并设为全局默认。
 *
 * 交给宿主**原子**完成：只有它握有明文 key，也避免前端「落条目 / 设默认」两步
 * 半途失败留下半截状态。回包给出新条目名；缺字段回空串，视图只需一个可显示的值。
 */
export async function importEnvModel(rpc: ModelRpc): Promise<{ name: string }> {
  const result = await rpc<{ name?: unknown }>("model/importEnv", {});
  return { name: typeof result?.name === "string" ? result.name : "" };
}

/** `model/upsert` 的入参：`api_key` 省略 = 保留宿主里已有的 key。 */
export type ModelUpsertInput = {
  name: string;
  provider: "anthropic" | "openai";
  api_url: string;
  model: string;
  api_key?: string;
};

/**
 * 新增或整体覆盖一条模型。
 *
 * `api_key` 只在调用方显式给出时才进参数：省略即「不改 key」——宿主把缺省与
 * `null` 都当作保留，但显式传 `undefined` 会被序列化丢掉语义，所以这里用
 * `!== undefined` 判断，把「保留」这一意图准确地表达成「不出现该字段」。
 */
export async function upsertModel(rpc: ModelRpc, input: ModelUpsertInput): Promise<void> {
  const params: Record<string, unknown> = {
    name: input.name,
    provider: input.provider,
    api_url: input.api_url,
    model: input.model,
  };
  if (input.api_key !== undefined) params.api_key = input.api_key;
  await rpc("model/upsert", params);
}

/** 按名字删除一条模型；宿主会一并清掉指向它的默认/子代理引用。 */
export async function deleteModel(rpc: ModelRpc, name: string): Promise<void> {
  await rpc("model/delete", { name });
}

/** 设全局默认模型；`null` 清除（回落到宿主配置的兜底）。 */
export async function setDefaultModel(rpc: ModelRpc, name: string | null): Promise<void> {
  await rpc("model/setDefault", { name });
}

/** 设子代理模型；`null` 清除。 */
export async function setSubagentModel(rpc: ModelRpc, name: string | null): Promise<void> {
  await rpc("model/setSubagent", { name });
}

/**
 * 设某个会话的模型覆盖；`null` 清除。
 *
 * 覆盖只作用于该 thread，与全局默认分开：用户常在一条会话里试模型，不该
 * 顺手改掉其它会话。
 *
 * 回包带回**解析后的生效模型串**（清单条目的 `model`，不是用户选中的显示名）。
 * 调用方据此回显"现在跑的是哪个真实模型"，而不是拿本地那份可能已被别的客户端
 * 改过的清单去猜——猜出来的名字会和真正生效的模型对不上。缺字段时回空串：
 * 视图只需一个可显示的值，不必让调用方处理 `undefined`。
 */
export async function setThreadModel(
  rpc: ModelRpc,
  threadId: string,
  name: string | null,
): Promise<{ model: string }> {
  const result = await rpc<{ model?: unknown }>("thread/setModel", { threadId, name });
  return { model: typeof result?.model === "string" ? result.model : "" };
}

/**
 * 该错误是否为「引用了不存在的模型」。
 *
 * 结构化码优先：宿主在 `data.code` 里放稳定的 `model_not_found`（数字码
 * `-32025`）。message 文本作为退路，免得码缺失时 UI 把「模型没了」误报成
 * 未知故障——这两者对用户是两个动作（换个模型 / 重试）。
 */
export function isModelNotFound(error: unknown): boolean {
  const code = (error as { data?: { code?: unknown } } | null)?.data?.code;
  if (code === "model_not_found") return true;
  const text = error instanceof Error ? error.message : String(error ?? "");
  return text.includes("model_not_found");
}
