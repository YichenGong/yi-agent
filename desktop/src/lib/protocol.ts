/**
 * Wire-protocol types for the `yi-agent app-server` JSON-RPC 2.0 interface.
 *
 * The shapes that mirror `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`
 * are **generated** by ts-rs into `./generated/`. This module only re-exports
 * them so existing `from "./lib/protocol"` imports keep working. Do not
 * hand-edit those — edit the Rust and re-run the bindings export.
 *
 * Two conventions still matter when reading the protocol:
 *
 * - Most `params` fields are `snake_case` (`thread_id`, `turn_id`, `item_id`,
 *   `input_tokens`, `output_tokens`), matching serde's default field naming.
 * - The `Item` union is tagged with a camelCase `type` discriminator
 *   (`userMessage` / `agentMessage` / `toolCall` / `userInterjection` /
 *   `thinking`).
 *
 * Everything below the re-exports has **no** counterpart in `protocol.rs`: those
 * are either wire shapes the server assembles with `json!` (no Rust type at all)
 * or types owned by other crates (`yi-agent-core`, `git_diff.rs`,
 * `yi-agent-tools`). They therefore stay hand-written here.
 *
 * This module is types only — no runtime logic.
 */

import type { ThreadMode } from "./threadPermissionMode";
/**
 * 下面这三个生成类型虽然只做再导出，但本文件保留下来的手写定义（在 `protocol.rs`
 * 里没有对应物）仍然**引用**它们，所以还要留一份本地类型绑定，否则那几处会报
 * `Cannot find name`。
 */
import type { AgentChild } from "./generated/AgentChild";
import type { AgentTraceRow } from "./generated/AgentTraceRow";
import type { ThreadStatus } from "./generated/ThreadStatus";

// Generated mirrors of `protocol.rs` (ts-rs). See the header for why the rest of
// this file is still hand-written.
export type { RequestId } from "./generated/RequestId";
export type { RpcError } from "./generated/RpcError";
export type { ToolStatus } from "./generated/ToolStatus";
export type { Attachment } from "./generated/Attachment";
export type { ImageRef } from "./generated/ImageRef";
export type { Item } from "./generated/Item";
export type { TurnStatus } from "./generated/TurnStatus";
export type { ThreadStatus } from "./generated/ThreadStatus";
export type { Notification } from "./generated/Notification";
export type { AgentChild } from "./generated/AgentChild";
export type { AgentTraceRow } from "./generated/AgentTraceRow";

/** `agent/children/list` 的响应。 */
export interface AgentChildrenListResult {
  children: AgentChild[];
}

/** `agent/trace/read` 与 `agent/trace/watch` 的响应。 */
export interface AgentTraceSnapshotResult {
  rows: AgentTraceRow[];
  highWaterId: number;
}

/** `agent/cancel/preview` 的响应：确认取消所需的 token 与它的有效期。 */
export interface AgentCancelPreviewResult {
  confirmationToken: string;
  taskIds: string[];
  expiresInSecs: number;
}

/// Why the turn is being retried, as reported by the server on `turn/retry`.
export type RetryCause =
  | "idle_stall"
  | "request_timeout"
  | "rate_limited"
  | "server_error"
  | "network_error";

export type PermissionKind = "Normal" | { Blacklisted: string };

export interface ApprovalRequest {
  id: string;
  params: {
    thread_id: string;
    turn_id: string;
    request_id: number;
    tool_name: string;
    tool_input: unknown;
    prefix_suggestion: string | null;
    kind: PermissionKind;
  };
}

export type Decision =
  | { decision: "allow_once" }
  | { decision: "always_allow_tool" }
  | { decision: "always_allow_prefix"; prefix: string }
  | { decision: "deny" };

/** A persisted thread as returned by `thread/list`. */
export interface ThreadSummary {
  thread_id: string;
  cwd: string;
  model: string;
  created_at: number;
  updated_at: number;
  title: string | null;
  /** Per-thread autonomy mode. 缺省(旧服务端/旧数据)视为 "normal";读取用 `?? "normal"`。 */
  permission_mode?: ThreadMode;
  /** 服务端权威状态；旧服务端缺省视为 "idle"。 */
  status?: ThreadStatus;
  /** 是否置于侧栏顶部的 Pinned 分区。旧服务端缺省视为 false。 */
  pinned?: boolean;
  /** 由看板创建时，该会话所属的项目根（绝对路径）。旧服务端缺省视为普通会话。 */
  board_project?: string;
  /** 由看板创建时，该会话对应的卡 id。旧服务端缺省视为普通会话。 */
  card_id?: string;
  /**
   * 该会话的模型引用（模型清单里的 `name`）。缺省/`null` = 跟随全局默认。
   * 旧服务端不发送此字段，读取时按 `null` 处理。
   */
  model_ref?: string | null;
  /**
   * 会话角色。`"lead"` = 该会话是所属项目的组长。缺省 = 普通会话
   * （旧服务端不发送此字段）。未知取值必须忽略，不得当成组长。
   */
  role?: string;
  /**
   * 颜色标签 id（`threadTags.ts` 的 `TAG_IDS` 之一）。缺省 / `null` = 无标签。
   * 旧服务端不发送此字段；读取时先过 `isValidTagId` 再渲染，脏值按无标签处理。
   */
  tag?: string | null;
  /**
   * 服务端的内容世代。旧服务端不发送此字段：读取时必须按"未知"处理
   * （保守全量 resume），绝不当作"一致"（spec §4.3）。`null` 与缺省同义。
   */
  content_epoch?: number | null;
  /** 服务端最后一条 item 的 id；`null` = 该会话从未有过 item。 */
  last_item_id?: string | null;
}

/** Token 用量(前端归一化后)。`cacheWrite` = 写入 cache,`cacheRead` = 命中 cache。 */
export interface Usage {
  model: string;
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

/** `workspace/list` 的单项。 */
export interface Workspace {
  path: string;
  exists: boolean;
  /** 该目录被关联到的主根（canonical 绝对路径）；null / 缺省 = 顶层目录。 */
  linked_to?: string | null;
}

/** `thread/listAll` 的一个目录分组。 */
export interface WorkspaceGroup {
  workspace: string;
  exists: boolean;
  threads: ThreadSummary[];
}

/**
 * `thread/clear` 的参数与结果。清空该 thread 的 agent 上下文并截断持久化日志，
 * 保留 thread 身份（标题 / cwd / 模型）。
 */
export interface ThreadClearParams {
  threadId: string;
}
export type ThreadClearResult = Record<string, never>;

/**
 * `thread/compact` 的参数与结果。`status` 三态：
 * - `compacted`   —— 已压缩
 * - `not_reduced` —— 历史太短，无需压缩（不是错误）
 * - `failed`      —— 压缩失败，`error` 带原因
 */
export interface ThreadCompactParams {
  threadId: string;
}
export interface ThreadCompactResult {
  status: "compacted" | "not_reduced" | "failed";
  error?: string;
}

export interface CommitInfo {
  sha: string;
  short: string;
  subject: string;
  author: string;
  timestamp: number;
}

export interface FileStat {
  path: string;
  status: string;
  additions: number;
  deletions: number;
  binary: boolean;
}

/** `thread/diff/read` 的默认响应。 */
export interface ThreadDiffResult {
  base: string | null;
  baseKind: string;
  mergeBase: string | null;
  commits: CommitInfo[];
  files: FileStat[];
  unifiedDiff: string;
  truncated: boolean;
}

/** `thread/diff/read` 的 `commit` / `path` 分支响应。 */
export interface DiffTextResult {
  unifiedDiff: string;
  truncated: boolean;
}

/**
 * 一个受管后台进程的状态。
 *
 * **注意线形状**：Rust 侧 `ProcessStatus` 带
 * `#[serde(tag = "state", rename_all = "snake_case")]`，是内部标记枚举，序列化成
 * **对象**（`{"state":"running"}`），不是裸字符串。`exited` 会带 `code`，
 * `failed_to_start` 会带 `reason`。读取状态一律用 `.state`。
 */
export interface ProcessStatus {
  state: string;
  /** 仅 `exited` 携带；`null` 表示进程没有退出码。 */
  code?: number | null;
  /** 仅 `failed_to_start` 携带。 */
  reason?: string;
}

/** `process/list` 的一项。镜像 Rust 的 `ManagedProcessSnapshot`（snake_case）。 */
export interface ProcessInfo {
  process_id: string;
  name: string | null;
  pid: number | null;
  command: string;
  cwd: string;
  status: ProcessStatus;
  ready: boolean;
  on_exit: "kill" | "keep";
  exit_code: number | null;
  elapsed_sec: number;
}

/** `process/list` 的响应。 */
export interface ProcessListResult {
  processes: ProcessInfo[];
}

/** `process/read` 的响应。`next_cursor` 是下一次增量读的游标。 */
export interface ProcessReadResult {
  process_id: string;
  name: string | null;
  stdout: string;
  stderr: string;
  next_cursor: number;
  truncated: boolean;
  status: ProcessStatus;
  ready: boolean;
}
