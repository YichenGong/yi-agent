import { useState } from "react";
import { looksMissingV1 } from "../lib/models";
import {
  applyOnboarding,
  dismissOnboarding,
  reasonLabel,
  testOnboarding,
  type ApplyInput,
  type OnboardingRpc,
} from "../lib/onboarding";

type Step = "welcome" | "provider" | "form";

/**
 * 向导收尾的结果（spec §4.7）。
 *
 * 落盘是单写 `models.json`：要么成功，要么宿主以 RPC 错误抛出（没有「部分成功」态）。
 * 保留这个类型是为了给将来可能的收尾信息留一个稳定的回调形状。
 */
export type OnboardingOutcome = { saved: boolean };

const DEFAULT_URL = {
  anthropic: "https://api.anthropic.com",
  openai: "https://api.openai.com/v1",
} as const;
/**
 * 连接测试的状态机。
 *
 * `passed` 是保存的唯一通行证；任一端点字段变化都把它打回 `untested`——
 * 否则用户测的是 A 配置、存进去的却是 B 配置。
 */
type TestState = "untested" | "testing" | "passed" | "failed";

export function OnboardingWizard({
  call,
  reasons = [],
  onDone,
}: {
  call?: OnboardingRpc;
  reasons?: string[];
  onDone: (result?: OnboardingOutcome) => void;
}) {
  const [step, setStep] = useState<Step>("welcome");
  const [provider, setProvider] = useState<"anthropic" | "openai">("anthropic");
  const [apiKey, setApiKey] = useState("");
  const [model, setModel] = useState("");
  const [apiUrl, setApiUrl] = useState("");
  const [testState, setTestState] = useState<TestState>("untested");
  const [testReason, setTestReason] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const dismiss = async () => {
    try {
      if (call) await dismissOnboarding(call);
    } finally {
      onDone();
    }
  };

  // 模型不回填默认值：它是一个没被确认过的猜测。地址留空则交由宿主补
  // provider 默认值（`import_settings_into_catalog` / `test_connection` 同源）。
  const input = (): ApplyInput => ({
    provider,
    model: model.trim(),
    api_url: apiUrl.trim(),
    api_key: apiKey,
  });

  /** 任一端点字段变化都作废已有的测试结论。 */
  const invalidateTest = () => {
    if (testState !== "untested") setTestState("untested");
    setTestReason(null);
  };

  const fieldsReady = model.trim() !== "" && apiKey.trim() !== "";
  const canSave = fieldsReady && testState === "passed";

  const runTest = async () => {
    if (!call) return;
    setBusy(true);
    setTestState("testing");
    setTestReason(null);
    setError(null);
    try {
      const result = await testOnboarding(call, input());
      if (result.ok) {
        setTestState("passed");
      } else {
        setTestState("failed");
        setTestReason(result.reason ?? "连接失败");
      }
    } catch (e) {
      setTestState("failed");
      setTestReason(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const save = async () => {
    if (!call) return;
    setBusy(true);
    setError(null);
    try {
      const result = await applyOnboarding(call, input());
      // `ok:false` 不是抛错，但与抛错同路：不 dismiss、不进主界面，把失败留在
      // 向导里内联显示。否则「没保存成功却看着像完成」会静默丢掉用户的配置。
      if (!result.ok) {
        setError("保存失败");
        return;
      }
      await dismissOnboarding(call);
      onDone({ saved: true });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-panel">
      <div className="w-[32rem] rounded-lg border border-line bg-panel p-6 text-fg">
        <div className="flex items-center justify-between">
          <h1 className="text-lg font-medium">欢迎使用 Yi-Agent</h1>
          <button
            type="button"
            onClick={() => void dismiss()}
            className="rounded px-2 py-0.5 text-xs text-fg-subtle hover:text-fg"
          >
            稍后设置
          </button>
        </div>

        {step === "welcome" && (
          <div className="mt-4">
            <p className="text-sm text-fg-muted">
              配置一个模型就能开始对话。所有信息存在本机，写入 `~/.yi-agent/models.json`。
            </p>
            {reasons.length > 0 && (
              <ul className="mt-2 list-disc pl-5 text-xs text-fg-subtle">
                {reasons.map((code) => (
                  <li key={code}>{reasonLabel(code)}</li>
                ))}
              </ul>
            )}
            <button
              type="button"
              onClick={() => setStep("provider")}
              className="mt-4 rounded-md border border-line-strong px-3 py-1.5 text-sm hover:text-fg"
            >
              开始
            </button>
          </div>
        )}

        {step === "provider" && (
          <div className="mt-4">
            <p className="text-sm text-fg-muted">选择 API 格式：</p>
            <div className="mt-2 flex gap-2">
              {(["anthropic", "openai"] as const).map((p) => (
                <button
                  key={p}
                  type="button"
                  onClick={() => {
                    setProvider(p);
                    invalidateTest();
                  }}
                  aria-pressed={provider === p}
                  className={`rounded-md border px-3 py-1.5 text-sm ${
                    provider === p ? "border-line-strong text-fg" : "border-line text-fg-muted"
                  }`}
                >
                  {p}
                </button>
              ))}
            </div>
            <button
              type="button"
              onClick={() => setStep("form")}
              className="mt-4 rounded-md border border-line-strong px-3 py-1.5 text-sm hover:text-fg"
            >
              下一步
            </button>
          </div>
        )}

        {step === "form" && (
          <div className="mt-4 flex flex-col gap-3">
            <label className="flex flex-col gap-1 text-xs text-fg-muted">
              API 密钥
              <input
                type="password"
                aria-required="true"
                value={apiKey}
                onChange={(e) => {
                  setApiKey(e.target.value);
                  invalidateTest();
                }}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              />
            </label>
            <label className="flex flex-col gap-1 text-xs text-fg-muted">
              模型标识
              <input
                aria-required="true"
                value={model}
                placeholder="例如 claude-sonnet-4-20250514 或 gpt-4o"
                onChange={(e) => {
                  setModel(e.target.value);
                  invalidateTest();
                }}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              />
            </label>
            <label className="flex flex-col gap-1 text-xs text-fg-muted">
              API 地址
              <input
                value={apiUrl}
                placeholder={DEFAULT_URL[provider]}
                onChange={(e) => {
                  setApiUrl(e.target.value);
                  invalidateTest();
                }}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              />
              {looksMissingV1(provider, apiUrl) && (
                <p role="status" className="text-xs text-amber-400">
                  OpenAI 兼容接口的地址通常以 /v1 结尾，请确认地址是否正确。
                </p>
              )}
            </label>

            {testState === "passed" && (
              <p role="status" className="text-xs text-emerald-400">
                连接正常
              </p>
            )}
            {testState === "failed" && testReason !== null && (
              <p role="alert" className="text-xs text-red-400">
                {testReason}
              </p>
            )}
            {error !== null && (
              <p role="alert" className="text-xs text-red-400">
                {error}
              </p>
            )}

            <p className="text-xs text-fg-subtle">
              API 密钥与模型标识为必填；本次只验证地址、密钥与模型标识，必须测试通过才能保存。
            </p>

            <div className="flex items-center gap-2">
              <button
                type="button"
                onClick={() => void runTest()}
                disabled={busy || !call || !fieldsReady}
                className="rounded-md border border-line px-3 py-1.5 text-sm text-fg-muted hover:text-fg disabled:opacity-50"
              >
                测试连接
              </button>
              <button
                type="button"
                onClick={() => void save()}
                disabled={busy || !call || !canSave}
                className="rounded-md border border-line-strong px-3 py-1.5 text-sm hover:text-fg disabled:opacity-50"
              >
                保存
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
