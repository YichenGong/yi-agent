/**
 * `onboarding/*` 的桌面端纯封装。
 *
 * 与 `models.ts` 同一层：不碰 React、不碰具体 `RpcClient`，只把「调用意图」
 * 翻成 `method` + `params`，并把宿主回包归一成可判定的形状。明文 key 只
 * 单向发送，从不回读。
 */
import type { RpcCall } from "./rpcCall";
import { RPC } from "./routes";

export type OnboardingRpc = RpcCall;

export type OnboardingStatus = {
  needed: boolean;
  dismissed: boolean;
  reasons: string[];
};

export type ApplyInput = {
  provider: "anthropic" | "openai";
  model: string;
  api_url: string;
  api_key: string;
};

export type ApplyResult = {
  ok: boolean;
  /** 落成的清单条目名（`onboarding/apply` 单写清单后的唯一产物）。 */
  name?: string;
};

export type TestResult = { ok: boolean; reason: string | null };

/** 把宿主给的缺失项代码翻成一句人话（向导首屏据此说清缺什么）。 */
export function reasonLabel(code: string): string {
  switch (code) {
    case "provider":
      return "未选择 API 格式";
    case "model":
      return "未设置模型标识";
    case "api_url":
      return "API 地址格式不正确";
    case "api_key":
      return "未设置 API 密钥";
    default:
      return code;
  }
}

/** 读引导状态。缺字段一律按「不需要引导」处理，绝不因宿主少发字段就弹向导。 */
export async function onboardingStatus(rpc: OnboardingRpc): Promise<OnboardingStatus> {
  const result = await rpc<Partial<OnboardingStatus>>(RPC.onboardingStatus, {});
  return {
    needed: result?.needed === true,
    dismissed: result?.dismissed === true,
    reasons: Array.isArray(result?.reasons) ? result.reasons : [],
  };
}

/** 单写模型清单。失败由宿主以 RPC 错误抛出（无「部分成功」态）。 */
export async function applyOnboarding(
  rpc: OnboardingRpc,
  input: ApplyInput,
): Promise<ApplyResult> {
  const result = await rpc<Partial<ApplyResult>>(RPC.onboardingApply, {
    provider: input.provider,
    model: input.model,
    api_url: input.api_url,
    api_key: input.api_key,
  });
  return {
    ok: result?.ok === true,
    name: typeof result?.name === "string" ? result.name : undefined,
  };
}

/** 探测连接。失败带可读原因；成功 reason 为 null。 */
export async function testOnboarding(rpc: OnboardingRpc, input: ApplyInput): Promise<TestResult> {
  const result = await rpc<{ ok?: unknown; reason?: unknown }>(RPC.onboardingTest, {
    provider: input.provider,
    model: input.model,
    api_url: input.api_url,
    api_key: input.api_key,
  });
  return {
    ok: result?.ok === true,
    reason: typeof result?.reason === "string" ? result.reason : null,
  };
}

/** 标记引导已结束（完成或「稍后设置」）。 */
export async function dismissOnboarding(rpc: OnboardingRpc): Promise<void> {
  await rpc(RPC.onboardingDismiss, {});
}
