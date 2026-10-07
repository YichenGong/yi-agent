/**
 * Wire-protocol types for the `yi-agent app-server` JSON-RPC 2.0 interface.
 *
 * These mirror the Rust definitions in
 * `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`. Two conventions
 * matter when reading them:
 *
 * - All `params` fields are `snake_case` (`thread_id`, `turn_id`, `item_id`,
 *   `input_tokens`, `output_tokens`), matching serde's default field naming.
 * - The `Item` union is tagged with a camelCase `type` discriminator
 *   (`userMessage` / `agentMessage` / `toolCall`).
 *
 * This module is types only — no runtime logic.
 */

import type { ThreadMode } from "./threadPermissionMode";

export type RequestId = number | string;

export interface RpcError {
  code: number;
  message: string;
  data?: unknown;
}

export type ToolStatus = "running" | "completed" | "failed";

export type Item =
  | { type: "userMessage"; id: string; text: string }
  /**
   * 中途追加的用户消息：属于当前 turn，但作为独立 item 渲染，避免与开启该
   * turn 的那条消息混为一谈。`id` 是服务端在 RPC 入口铸的 `interjection_id`。
   */
  | { type: "user_interjection"; id: string; text: string }
  | { type: "agentMessage"; id: string; text: string }
  | {
      type: "toolCall";
      id: string;
      call_id: string;
      name: string;
      input: unknown;
      status: ToolStatus;
      result?: string;
    };

export type TurnStatus = "completed" | "interrupted" | "failed";

/** thread 级实时状态；服务端权威（`thread/status/updated` / `thread/list(:All)`）。 */
export type ThreadStatus = "idle" | "running" | "awaiting_approval";

export type Notification =
  | { method: "thread/started"; params: { thread_id: string; cwd: string; model: string } }
  | { method: "thread/status/updated"; params: { thread_id: string; status: ThreadStatus } }
  | { method: "turn/started"; params: { thread_id: string; turn_id: string } }
  | { method: "item/started"; params: { thread_id: string; item: Item } }
  | { method: "item/delta"; params: { thread_id: string; item_id: string; delta: string } }
  | { method: "item/completed"; params: { thread_id: string; item: Item } }
  | { method: "items/completed"; params: { thread_id: string; items: Item[] } }
  | {
      method: "turn/completed";
      params: { thread_id: string; turn_id: string; status: TurnStatus; error?: string };
    }
  | {
      method: "turn/retry";
      params: {
        thread_id: string;
        turn_id: string;
        attempt: number;
        max: number;
        cause: "idle_stall" | "request_timeout";
      };
    }
  | {
      method: "thread/tokenUsage/updated";
      params: {
        thread_id: string;
        model: string;
        input_tokens: number;
        output_tokens: number;
        /** 缺省(旧服务端)按 0 处理。 */
        cache_creation_input_tokens?: number;
        cache_read_input_tokens?: number;
      };
    }
  | {
      method: "turn/interjectionsReturned";
      params: { thread_id: string; turn_id: string; items: string[] };
    }
  | {
      method: "agent/children/updated";
      params: { threadId: string; children: AgentChild[] };
    }
  | {
      method: "agent/trace/event";
      params: { threadId: string; taskId: string; row: AgentTraceRow };
    }
  | { method: "ui/settings/updated"; params: { theme: string } }
  | {
      method: "ui/gitDiff/focus";
      params: { threadId: string | null; base: string | null; note: string | null };
    }
  | { method: "error"; params: { message: string } };

/**
 * 该对话名下一个子 agent 的列表项。
 *
 * 字段是 camelCase：`agent/*` 是与 `thread/` `turn/` 并列的新命名空间,其参数
 * 由 app-server 直接序列化 `AgentChild`,故不沿用旧命名空间的 snake_case。
 */
export interface AgentChild {
  taskId: string;
  objective?: string;
  state: string;
  lastStep?: string;
  /** 该子任务自己的父任务;顶层子任务缺省。用于下钻时只列出自己的直接子任务。 */
  parentTaskId?: string;
}

/** 一条子 agent 轨迹行,原样透传 daemon 的持久化行。 */
export interface AgentTraceRow {
  eventId: number;
  taskId: string;
  kind: "assistant_text" | "tool_call" | "tool_result" | "state_note" | string;
  payloadJson: string;
}

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
export type RetryCause = "idle_stall" | "request_timeout";

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
