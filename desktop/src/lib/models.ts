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

/** `model/list` 的完整回包：清单 + 两个全局引用。 */
export type ModelList = {
  models: ModelEntryView[];
  default_model: string | null;
  subagent_model: string | null;
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
  };
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
 */
export async function setThreadModel(
  rpc: ModelRpc,
  threadId: string,
  name: string | null,
): Promise<void> {
  await rpc("thread/setModel", { threadId, name });
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
