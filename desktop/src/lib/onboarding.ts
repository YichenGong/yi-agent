/**
 * `onboarding/*` 的桌面端纯封装。
 *
 * 与 `models.ts` 同一层：不碰 React、不碰具体 `RpcClient`，只把「调用意图」
 * 翻成 `method` + `params`，并把宿主回包归一成可判定的形状。明文 key 只
 * 单向发送，从不回读。
 */
export type OnboardingRpc = <T = unknown>(method: string, params: unknown) => Promise<T>;

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
  env_written: boolean;
  imported: boolean;
  import_error?: string;
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
  const result = await rpc<Partial<OnboardingStatus>>("onboarding/status", {});
  return {
    needed: result?.needed === true,
    dismissed: result?.dismissed === true,
    reasons: Array.isArray(result?.reasons) ? result.reasons : [],
  };
}

/** 写 `.env` 并尝试收边到清单。部分成功（imported=false）也返回 `ok`。 */
export async function applyOnboarding(
  rpc: OnboardingRpc,
  input: ApplyInput,
): Promise<ApplyResult> {
  const result = await rpc<Partial<ApplyResult>>("onboarding/apply", {
    provider: input.provider,
    model: input.model,
    api_url: input.api_url,
    api_key: input.api_key,
  });
  return {
    ok: result?.ok === true,
    env_written: result?.env_written === true,
    imported: result?.imported === true,
    import_error: typeof result?.import_error === "string" ? result.import_error : undefined,
  };
}

/** 探测连接。失败带可读原因；成功 reason 为 null。 */
export async function testOnboarding(rpc: OnboardingRpc, input: ApplyInput): Promise<TestResult> {
  const result = await rpc<{ ok?: unknown; reason?: unknown }>("onboarding/test", {
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
  await rpc("onboarding/dismiss", {});
}
