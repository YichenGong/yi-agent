import { useCallback, useEffect, useState } from "react";
import {
  parseTestResult,
  parseWebSearchView,
  type Freshness,
  type WebSearchCall,
} from "../lib/websearch";

const FRESHNESS_CHOICES: Array<{ value: Freshness; label: string }> = [
  { value: "noLimit", label: "不限时间" },
  { value: "oneDay", label: "最近一天" },
  { value: "oneWeek", label: "最近一周" },
  { value: "oneMonth", label: "最近一月" },
  { value: "oneYear", label: "最近一年" },
];

/** 上次读/存时的值，用来算出「用户改了哪几项」。 */
interface Baseline {
  enabled: boolean;
  baseUrl: string;
  count: number;
  freshness: Freshness;
}

const EMPTY_BASELINE: Baseline = {
  enabled: true,
  baseUrl: "https://api.bocha.cn/v1",
  count: 25,
  freshness: "noLimit",
};

/**
 * 「网页搜索」页。
 *
 * 只有 Bocha 一个引擎卡片——`engines` 在配置里是 map，多引擎时这里改成横向
 * 排列即可，不必动数据层。
 *
 * 保存与测试都走宿主接缝（`websearch/set` / `websearch/test`）。密钥框从不回填
 * 明文：服务端只给掩码，空值等于「不改」。
 */
export function SettingsWebSearchTab({ call }: { call: WebSearchCall }) {
  const [enabled, setEnabled] = useState(true);
  const [apiKey, setApiKey] = useState("");
  const [keyMask, setKeyMask] = useState("");
  const [hasKey, setHasKey] = useState(false);
  const [baseUrl, setBaseUrl] = useState(EMPTY_BASELINE.baseUrl);
  const [count, setCount] = useState(EMPTY_BASELINE.count);
  const [freshness, setFreshness] = useState<Freshness>(EMPTY_BASELINE.freshness);
  const [baseline, setBaseline] = useState<Baseline>(EMPTY_BASELINE);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [notice, setNotice] = useState<{ ok: boolean; text: string } | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const view = parseWebSearchView(await call("websearch/get", {}));
      const bocha = view.engines.bocha;
      setEnabled(view.enabled);
      // 明文从不下发，输入框每次读回都清空，只留掩码当 placeholder。
      setApiKey("");
      setKeyMask(bocha.apiKeyMasked);
      setHasKey(bocha.hasKey);
      setBaseUrl(bocha.baseUrl);
      setCount(bocha.count);
      setFreshness(bocha.freshness);
      setBaseline({
        enabled: view.enabled,
        baseUrl: bocha.baseUrl,
        count: bocha.count,
        freshness: bocha.freshness,
      });
    } catch (e) {
      setNotice({ ok: false, text: `读取配置失败：${String(e)}` });
    } finally {
      setLoading(false);
    }
  }, [call]);

  useEffect(() => {
    void load();
  }, [load]);

  const save = async () => {
    // 只传真正变了的字段：既让服务端不必猜测意图，也让「掩码框为空表示不改」
    // 这条规则自然成立——没改就不会有 `api_key` 这一项。
    const enginePatch: Record<string, unknown> = {};
    if (apiKey.trim()) enginePatch.api_key = apiKey.trim();
    if (baseUrl !== baseline.baseUrl) enginePatch.base_url = baseUrl;
    if (count !== baseline.count) enginePatch.count = count;
    if (freshness !== baseline.freshness) enginePatch.freshness = freshness;

    const body: Record<string, unknown> = {};
    if (enabled !== baseline.enabled) body.enabled = enabled;
    if (Object.keys(enginePatch).length > 0) body.engines = { bocha: enginePatch };

    if (Object.keys(body).length === 0) {
      setNotice({ ok: true, text: "没有需要保存的改动" });
      return;
    }

    setSaving(true);
    setNotice(null);
    try {
      await call("websearch/set", body);
      setNotice({ ok: true, text: "已保存" });
      await load();
    } catch (e) {
      setNotice({ ok: false, text: `保存失败：${String(e)}` });
    } finally {
      setSaving(false);
    }
  };

  const runTest = async () => {
    setTesting(true);
    setNotice(null);
    try {
      // 草稿 key 优先：用户刚填还没保存时也能立刻验证。
      const params = apiKey.trim() ? { api_key: apiKey.trim() } : {};
      const result = parseTestResult(await call("websearch/test", params));
      setNotice({
        ok: result.ok,
        text: `${result.message}（${result.elapsedMs} ms）`,
      });
    } catch (e) {
      setNotice({ ok: false, text: `测试失败：${String(e)}` });
    } finally {
      setTesting(false);
    }
  };

  if (loading) return <section className="p-5 text-sm text-fg-muted">读取中…</section>;

  return (
    <section className="p-5">
      <label className="flex items-center gap-3 text-sm">
        <input
          type="checkbox"
          aria-label="启用网页搜索"
          checked={enabled}
          onChange={(e) => setEnabled(e.target.checked)}
        />
        <span>启用网页搜索</span>
      </label>

      <h3 className="mt-6 text-sm font-medium text-fg">搜索引擎</h3>
      <div className="mt-3 rounded-md border border-line p-4">
        <div className="flex items-center justify-between">
          <span className="text-sm font-medium text-fg">Bocha</span>
          <span className="rounded bg-raised px-2 py-0.5 text-xs text-fg-muted">当前引擎</span>
        </div>

        <label className="mt-4 block text-xs text-fg-muted" htmlFor="ws-api-key">
          API Key
        </label>
        <input
          id="ws-api-key"
          type="password"
          aria-label="API Key"
          value={apiKey}
          placeholder={keyMask || "请输入 API Key"}
          onChange={(e) => setApiKey(e.target.value)}
          className="mt-1 w-full rounded border border-line bg-base px-3 py-1.5 text-sm"
        />

        <label className="mt-4 block text-xs text-fg-muted" htmlFor="ws-base-url">
          Base URL
        </label>
        <input
          id="ws-base-url"
          aria-label="Base URL"
          value={baseUrl}
          onChange={(e) => setBaseUrl(e.target.value)}
          className="mt-1 w-full rounded border border-line bg-base px-3 py-1.5 text-sm"
        />

        <label className="mt-4 block text-xs text-fg-muted" htmlFor="ws-count">
          结果条数
        </label>
        <input
          id="ws-count"
          type="number"
          min={1}
          max={50}
          aria-label="结果条数"
          value={count}
          onChange={(e) => setCount(Number(e.target.value))}
          className="mt-1 w-32 rounded border border-line bg-base px-3 py-1.5 text-sm"
        />

        <label className="mt-4 block text-xs text-fg-muted" htmlFor="ws-freshness">
          时效范围
        </label>
        <select
          id="ws-freshness"
          aria-label="时效范围"
          value={freshness}
          onChange={(e) => setFreshness(e.target.value as Freshness)}
          className="mt-1 w-48 rounded border border-line bg-base px-3 py-1.5 text-sm"
        >
          {FRESHNESS_CHOICES.map((c) => (
            <option key={c.value} value={c.value}>
              {c.label}
            </option>
          ))}
        </select>

        {!hasKey && !apiKey.trim() && (
          <p role="alert" className="mt-4 text-xs text-amber-500">
            未配置 API Key，网页搜索将不可用。
          </p>
        )}
      </div>

      <div className="mt-6 flex items-center gap-3">
        <button
          type="button"
          onClick={() => void save()}
          disabled={saving}
          className="rounded bg-accent px-4 py-1.5 text-sm text-white disabled:opacity-50"
        >
          {saving ? "保存中…" : "保存"}
        </button>
        <button
          type="button"
          onClick={() => void runTest()}
          disabled={testing}
          className="rounded border border-line px-4 py-1.5 text-sm disabled:opacity-50"
        >
          {testing ? "测试中…" : "测试连接"}
        </button>
      </div>

      {notice && (
        <p
          role={notice.ok ? "status" : "alert"}
          className={`mt-3 text-xs ${notice.ok ? "text-accent" : "text-amber-500"}`}
        >
          {notice.text}
        </p>
      )}
    </section>
  );
}
