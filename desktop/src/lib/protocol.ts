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
  | { method: "error"; params: { message: string } };

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
