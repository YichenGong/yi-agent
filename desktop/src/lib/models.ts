/**
 * `model/*` 与 `thread/setModel` 的桌面端纯封装。
 *
 * 与 `boardIndex.ts` / `pluginSettings.ts` 同一层：不碰 React、不碰具体
 * `RpcClient`，只把「调用意图」翻译成 `method` + `params`，并把宿主错误翻成
 * 一句可判定的谓词。形状取宿主 JSON-RPC 的线格式（`snake_case`），桌面端
 * 其余代码按字段直用，不做二次映射。
 */
import type { RpcCall } from "./rpcCall";
import { RPC } from "./routes";

export type ModelRpc = RpcCall;

/** 模型清单里的一条，与宿主 `model/list` 返回的条目形状一致（key 只回掩码）。 */
export type ModelEntryView = {
  name: string;
  provider: "anthropic" | "openai";
  api_url: string;
  model: string;
  has_key: boolean;
  api_key_masked: string;
  /**
   * 该条目声明的接口风格：`auto`（默认）/ `responses` / `chat`。**缺省/null = 未声明**
   * （回落 `auto` 的解析语义）；旧服务端不发送此字段，读取时按 `null` 处理。
   */
  api_style?: string | null;
  /**
   * 该条目声明的推理档位（七个白名单值之一）。**缺省/null = 未声明**，请求就不带
   * `reasoning_effort`，由模型自行决定。
   */
  reasoning_effort?: string | null;
  /**
   * 该条目在各通道上**实测**支持哪些档位（加模型/改连接后由宿主后台探测）。
   *
   * **缺省/null = 能力未知**（还没测、或整轮探测失败）。未知时必须平铺七档——
   * 这与探测功能上线前的行为一致；把它当成「零个档位」会让用户无从下手。
   */
  reasoning_capabilities?: ReasoningCapabilities | null;
};

/**
 * 单条通道的探测结论。
 *
 * `error` 非空 = 这一轮没得出可归因的结论（鉴权失败、超时、基线被拒），此时
 * `supported` 没有意义，读取方一律按「未知」处理。
 */
export type ChannelCapability = {
  supported: string[];
  /** epoch 秒。`0` = 老数据/坏数据，只用于展示，不参与判定。 */
  probed_at: number;
  error?: string | null;
};

/** 三个通道各自的能力；缺省 = 该通道没测过。 */
export type ReasoningCapabilities = {
  chat?: ChannelCapability | null;
  responses?: ChannelCapability | null;
  anthropic?: ChannelCapability | null;
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
  const result = await rpc<Partial<ModelList>>(RPC.modelList, {});
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
  const result = await rpc<{ name?: unknown }>(RPC.modelImportEnv, {});
  return { name: typeof result?.name === "string" ? result.name : "" };
}

/** `model/upsert` 的入参：`api_key` 省略 = 保留宿主里已有的 key。 */
export type ModelUpsertInput = {
  name: string;
  provider: "anthropic" | "openai";
  api_url: string;
  model: string;
  api_key?: string;
  /** 省略 = 保留宿主里已有的声明（与 `api_key` 同写法）。 */
  api_style?: string;
  /** 省略 = 保留宿主里已有的声明（与 `api_key` 同写法）。 */
  reasoning_effort?: string;
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
  // 同样只在显式给出时才进参数：省略 = 保留宿主里已有的声明。UI 未动这两个
  // 字段时（`undefined`），写入不携带它们，既有条目因此零改动。
  if (input.api_style !== undefined) params.api_style = input.api_style;
  if (input.reasoning_effort !== undefined) params.reasoning_effort = input.reasoning_effort;
  await rpc(RPC.modelUpsert, params);
}

/** 按名字删除一条模型；宿主会一并清掉指向它的默认/子代理引用。 */
export async function deleteModel(rpc: ModelRpc, name: string): Promise<void> {
  await rpc(RPC.modelDelete, { name });
}

/** 设全局默认模型；`null` 清除（回落到宿主配置的兜底）。 */
export async function setDefaultModel(rpc: ModelRpc, name: string | null): Promise<void> {
  await rpc(RPC.modelSetDefault, { name });
}

/** 设子代理模型；`null` 清除。 */
export async function setSubagentModel(rpc: ModelRpc, name: string | null): Promise<void> {
  await rpc(RPC.modelSetSubagent, { name });
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
  const result = await rpc<{ model?: unknown }>(RPC.threadSetModel, { threadId, name });
  return { model: typeof result?.model === "string" ? result.model : "" };
}

/** `thread/setReasoningEffort` 的回包：`effort: null` = 已清除覆盖。 */
export type ThreadEffortResult = { effort: string | null };

/**
 * 设某个会话的 reasoning effort 覆盖；`null` 清除（回落清单条目默认）。
 *
 * 与 `setThreadModel` 同族：覆盖只作用于该 thread。回包 `{ ok, effort }` 带回
 * **已实际应用**的档位（服务端在重建 agent 成功后才落盘、才回包），调用方据此
 * 回显，而不是拿本地选择当事实。host 报 null/缺省即「无覆盖」。 */
export async function setThreadReasoningEffort(
  rpc: ModelRpc,
  threadId: string,
  effort: string | null,
): Promise<ThreadEffortResult> {
  const result = await rpc<{ effort?: unknown }>(RPC.threadSetReasoningEffort, {
    threadId,
    effort,
  });
  return { effort: typeof result?.effort === "string" ? result.effort : null };
}

/**
 * 用当前草稿值（未保存也能测）发一次最小探测。
 *
 * `api_key` 省略即「让服务端按 `name` 取已存明文 key」——桌面端从不持有明文，
 * 与 `upsertModel` 的「省略即保留」同一条安全约定。回包 `ok/reason` 缺字段时
 * 保守地当作失败，避免把「宿主少发字段」误报成连接成功。
 */
export async function testModel(
  rpc: ModelRpc,
  input: { provider: string; model: string; api_url: string; api_key?: string; name?: string },
): Promise<{ ok: boolean; reason: string | null }> {
  const params: Record<string, unknown> = {
    provider: input.provider,
    model: input.model,
    api_url: input.api_url,
  };
  if (input.api_key !== undefined) params.api_key = input.api_key;
  else if (input.name !== undefined) params.name = input.name;
  const result = await rpc<{ ok?: unknown; reason?: unknown }>(RPC.modelTest, params);
  return {
    ok: result?.ok === true,
    reason: typeof result?.reason === "string" ? result.reason : null,
  };
}

/**
 * 启动某个模型条目的能力探测。
 *
 * 只**启动**：宿主立即回 `started`，真实结果最长 30 秒后经
 * `model/capabilitiesChanged` 广播。`started: false` = 同名条目已有一轮在跑
 * （去重），此时等广播即可，不必重试。
 */
export async function probeModel(
  rpc: ModelRpc,
  name: string,
): Promise<{ started: boolean }> {
  const result = await rpc<{ started?: unknown }>(RPC.modelProbe, { name });
  return { started: result?.started === true };
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

/**
 * OpenAI 兼容接口的地址通常以 `/v1` 结尾。
 *
 * 只做启发式提醒，不下硬结论：`/openai/v1` 这类带前缀的地址同样满足，
 * 因此判定用「去尾部空白与斜杠后是否以 `/v1` 结尾」，而不是「路径里含 /v1」。
 * 写错的后果是 404，而宿主会把它翻成「模型标识不被该地址接受」——这条
 * 提示会误导用户去改模型名，所以必须在保存前提醒。
 *
 * 空白 url **不**提示：它的语义是"用 provider 默认地址"（落盘时补
 * `https://api.openai.com/v1`），报警告只会制造噪音。
 */
export function looksMissingV1(provider: string, apiUrl: string): boolean {
  if (provider !== "openai") return false;
  const trimmed = apiUrl.trim();
  if (trimmed === "") return false;
  return !trimmed.replace(/\/+$/, "").endsWith("/v1");
}
