/**
 * 桌面与宿主之间的 MCP 通道。形状与 `skills.ts` / `pluginSettings.ts` 一致。
 *
 * 服务端返回的是手写 `json!`（不走 ts-rs DTO），故本模块做一次形状收敛：坏行丢掉、
 * 缺失字段补安全空值，绝不把半成品往下传——面板宁可少列，也不能因为一条脏数据
 * 整页崩掉。
 */

/** 泛型形状：调用方可自选返回类型。 */
export type McpRpc = <T = unknown>(method: string, params: unknown) => Promise<T>;

/** 非泛型接缝（宿主与测试用）：`vi.fn` 推断不出泛型函数，直接赋给 `McpRpc` 会 TS2322。 */
export type McpCall = (method: string, params: unknown) => Promise<unknown>;

/** 工具清单里的一个远端工具（不含参数 schema）。 */
export interface McpToolSummary {
  name: string;
  description: string;
  read_only: boolean;
}

export interface McpServerView {
  name: string;
  kind: "stdio" | "http";
  /** 文件里的该 server 开关默认值（只读展示，面板不写回）。 */
  enabled: boolean;
  /** stdio：可执行文件；http 下为 null。 */
  command: string | null;
  args: string[];
  /** http：endpoint；stdio 下为 null。 */
  url: string | null;
  /** env 的**键名**（值永不下发）。 */
  envKeys: string[];
  /** headers 的**键名**（值永不下发）。 */
  headerKeys: string[];
  /** `null` = 尚未探测（缓存缺失或指纹不匹配）；`[]` = 探测过但没有工具。 */
  tools: McpToolSummary[] | null;
  toolCount: number;
  readOnlyCount: number;
}

export interface McpList {
  /** `false` = 该项目没有 `.yi-agent/mcp.json`，是正常态而非错误。 */
  configured: boolean;
  /** 配置文件的真实解析路径（供「如何添加」文案用，不硬编码）。 */
  path: string;
  /** 文件里的顶层总开关默认值。 */
  enabled: boolean;
  servers: McpServerView[];
}

function asStringArray(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((v): v is string => typeof v === "string") : [];
}

function asOptString(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function asTool(row: unknown): McpToolSummary | undefined {
  if (typeof row !== "object" || row === null) return undefined;
  const r = row as Record<string, unknown>;
  if (typeof r.name !== "string" || r.name === "") return undefined;
  return {
    name: r.name,
    description: typeof r.description === "string" ? r.description : "",
    read_only: r.read_only === true,
  };
}

function asServer(row: unknown): McpServerView | undefined {
  if (typeof row !== "object" || row === null) return undefined;
  const r = row as Record<string, unknown>;
  if (typeof r.name !== "string" || r.name === "") return undefined;
  // 传输只认两种；未知值整行丢弃，而不是猜一个默认——猜错会把 http 的 endpoint
  // 当成 stdio 命令显示。
  if (r.kind !== "stdio" && r.kind !== "http") return undefined;
  const rawTools = r.tools;
  const tools =
    rawTools === null || rawTools === undefined
      ? null
      : (Array.isArray(rawTools) ? rawTools : [])
          .map(asTool)
          .filter((t): t is McpToolSummary => t !== undefined);
  return {
    name: r.name,
    kind: r.kind,
    enabled: r.enabled !== false,
    command: asOptString(r.command),
    args: asStringArray(r.args),
    url: asOptString(r.url),
    envKeys: asStringArray(r.env_keys),
    headerKeys: asStringArray(r.header_keys),
    tools,
    toolCount: typeof r.tool_count === "number" ? r.tool_count : 0,
    readOnlyCount: typeof r.read_only_count === "number" ? r.read_only_count : 0,
  };
}

/** 列出某个项目的 MCP server。`project` 缺省时由服务端回落宿主 workdir。 */
export async function listMcpServers(rpc: McpRpc, project?: string): Promise<McpList> {
  const result = await rpc<{
    configured?: unknown;
    path?: unknown;
    enabled?: unknown;
    servers?: unknown;
  }>("mcp/list", { ...(project ? { project } : {}) });
  const rows = Array.isArray(result?.servers) ? result.servers : [];
  return {
    configured: result?.configured === true,
    path: typeof result?.path === "string" ? result.path : "",
    enabled: result?.enabled === true,
    // 服务端按名字排序（BTreeMap）；这里不重排，保持与服务端一致。
    servers: rows.map(asServer).filter((s): s is McpServerView => s !== undefined),
  };
}