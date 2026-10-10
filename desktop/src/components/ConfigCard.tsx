/**
 * `/config` 的输出卡片。
 *
 * `config/read` 的 `redacted_view()` 是一组机器字段：`sandbox` 是 Rust 的 `{:?}`
 * 名、布尔是 `true/false`、密钥是掩码。这里把它们翻译成给人看的一行行键值，
 * 按「模型与服务 / 运行环境 / 限额」三组排进一张卡片——取代此前挤成一段等宽
 * 文本的灰色 notice。
 */

/** 布尔 → 中文；非布尔（缺字段）落回破折号。 */
function yesNo(v: unknown): string {
  return v === true ? "是" : v === false ? "否" : EM_DASH;
}

/**
 * 沙箱模式：Rust `{:?}` 名 → 线协议的 kebab 名。
 *
 * 取值与 `yi-agent-tools::SandboxMode::as_str` 一致；查不到就用原串，未来新增模式
 * 时不丢信息。
 */
const SANDBOX_LABELS: Record<string, string> = {
  ReadOnly: "read-only",
  WorkspaceWrite: "workspace-write",
  DangerFullAccess: "danger-full-access",
};

function sandboxLabel(v: unknown): string {
  if (typeof v === "string" && Object.prototype.hasOwnProperty.call(SANDBOX_LABELS, v)) {
    return SANDBOX_LABELS[v];
  }
  return scalar(v);
}

/**
 * 密钥状态。只说「有没有」——原文从不经过桌面端（服务端已脱敏成 `""` / `"***"`）。
 * 与 `SettingsModelsTab.effectiveKeyLabel` 同精神：这是状态行，不是凭证面板。
 */
function apiKeyLabel(v: unknown): string {
  if (v === undefined || v === null) return EM_DASH;
  return v === "" ? "未设置" : "已设置（***）";
}

/** 缺字段（`undefined` / `null`）显示破折号，其余原样 `String()`。 */
function scalar(v: unknown): string {
  return v === undefined || v === null ? EM_DASH : String(v);
}

const EM_DASH = "—";

type Row = { label: string; value: (cfg: Record<string, unknown>) => string };
type Group = { title: string; rows: Row[] };

/**
 * 卡片呈现的全部字段。静态表即契约：未知键被忽略，所以配置结构演进时不会把
 * 机器键名摊到界面上。
 */
const GROUPS: Group[] = [
  {
    title: "模型与服务",
    rows: [
      { label: "Provider", value: (c) => scalar(c.provider) },
      { label: "API URL", value: (c) => scalar(c.api_url) },
      { label: "API Key", value: (c) => apiKeyLabel(c.api_key) },
      { label: "模型", value: (c) => scalar(c.model) },
    ],
  },
  {
    title: "运行环境",
    rows: [
      { label: "工作目录", value: (c) => scalar(c.workdir) },
      { label: "沙箱", value: (c) => sandboxLabel(c.sandbox) },
      { label: "沙箱可提升", value: (c) => yesNo(c.sandbox_promotable) },
      { label: "YOLO", value: (c) => yesNo(c.yolo) },
    ],
  },
  {
    title: "限额",
    rows: [
      { label: "最大轮次", value: (c) => scalar(c.max_turns) },
      { label: "常驻子代理", value: (c) => scalar(c.max_resident_subagents) },
      { label: "直接子代理", value: (c) => scalar(c.max_direct_children) },
      { label: "压缩阈值", value: (c) => scalar(c.compact_threshold) },
    ],
  },
];

export function ConfigCard({
  config,
  error,
}: {
  config?: Record<string, unknown> | null;
  error?: string | null;
}) {
  return (
    <div
      data-testid="config-card"
      className="my-2 self-stretch rounded-lg border border-line-strong bg-surface px-3 py-2 text-sm"
    >
      <div className="mb-1 font-medium text-fg">当前配置</div>
      {error != null ? (
        <div className="text-red-300">{`读取配置失败: ${error}`}</div>
      ) : (
        <div className="flex flex-col gap-2">
          {GROUPS.map((group) => (
            <div key={group.title} className="flex flex-col gap-0.5">
              <div className="text-xs text-fg-subtle">{group.title}</div>
              {group.rows.map((row) => (
                <div key={row.label} className="flex gap-3">
                  <span className="w-24 shrink-0 text-fg-muted">{row.label}</span>
                  <span className="min-w-0 break-all text-fg">
                    {row.value(config ?? {})}
                  </span>
                </div>
              ))}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
