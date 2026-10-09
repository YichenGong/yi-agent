import { useState } from "react";
import {
  applyOnboarding,
  dismissOnboarding,
  reasonLabel,
  testOnboarding,
  type ApplyInput,
  type OnboardingRpc,
} from "../lib/onboarding";

type Step = "welcome" | "provider" | "form";

const DEFAULT_URL = {
  anthropic: "https://api.anthropic.com",
  openai: "https://api.openai.com",
} as const;
const DEFAULT_MODEL = {
  anthropic: "claude-sonnet-4-20250514",
  openai: "gpt-4o",
} as const;

export function OnboardingWizard({
  call,
  reasons = [],
  onDone,
}: {
  call?: OnboardingRpc;
  reasons?: string[];
  onDone: () => void;
}) {
  const [step, setStep] = useState<Step>("welcome");
  const [provider, setProvider] = useState<"anthropic" | "openai">("anthropic");
  const [apiKey, setApiKey] = useState("");
  const [model, setModel] = useState("");
  const [apiUrl, setApiUrl] = useState("");
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

  const input = (): ApplyInput => ({
    provider,
    model: model.trim() || DEFAULT_MODEL[provider],
    api_url: apiUrl.trim() || DEFAULT_URL[provider],
    api_key: apiKey,
  });

  const runTest = async () => {
    if (!call) return;
    setBusy(true);
    setTestReason(null);
    setError(null);
    try {
      const result = await testOnboarding(call, input());
      setTestReason(result.ok ? null : result.reason ?? "连接失败");
    } catch (e) {
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
      await applyOnboarding(call, input());
      await dismissOnboarding(call);
      onDone();
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
              配置一个模型就能开始对话。所有信息存在本机，只会写入你的全局 .env。
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
                  onClick={() => setProvider(p)}
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
                value={apiKey}
                onChange={(e) => setApiKey(e.target.value)}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              />
            </label>
            <label className="flex flex-col gap-1 text-xs text-fg-muted">
              模型标识
              <input
                value={model}
                placeholder={DEFAULT_MODEL[provider]}
                onChange={(e) => setModel(e.target.value)}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              />
            </label>
            <label className="flex flex-col gap-1 text-xs text-fg-muted">
              API 地址
              <input
                value={apiUrl}
                placeholder={DEFAULT_URL[provider]}
                onChange={(e) => setApiUrl(e.target.value)}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              />
            </label>

            {testReason !== null && (
              <p role="alert" className="text-xs text-red-400">
                {testReason}
              </p>
            )}
            {error !== null && (
              <p role="alert" className="text-xs text-red-400">
                {error}
              </p>
            )}

            <div className="flex items-center gap-2">
              <button
                type="button"
                onClick={() => void runTest()}
                disabled={busy || !call}
                className="rounded-md border border-line px-3 py-1.5 text-sm text-fg-muted hover:text-fg disabled:opacity-50"
              >
                测试连接
              </button>
              <button
                type="button"
                onClick={() => void save()}
                disabled={busy || !call}
                className="rounded-md border border-line-strong px-3 py-1.5 text-sm hover:text-fg disabled:opacity-50"
              >
                {testReason !== null ? "仍然保存" : "保存"}
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
