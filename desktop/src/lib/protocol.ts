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

/** 一条消息附带的文档元数据（内容不进协议，agent 用 read_document 读）。 */
export interface Attachment {
  name: string;
  /** 相对工作区根的路径，形如 `.yi-agent/attachments/<thread_id>/<hash>-<name>`。 */
  path: string;
  mime?: string;
  size: number;
}

/**
 * 一条消息附带的图片引用。**内容不进协议**：只有定位与展示所需的元数据，
 * 字节由 `image/read` 按需分片取。
 */
export interface ImageRef {
  path: string;
  media_type: string;
  /** 图片文件字节数；工具结果里的引用为 0，意为「未知」（非 0 字节）。 */
  size: number;
  detail?: string;
}

export type Item =
  | {
      type: "userMessage";
      id: string;
      text: string;
      attachments?: Attachment[];
      images?: ImageRef[];
    }
  /**
   * 中途追加的用户消息：属于当前 turn，但作为独立 item 渲染，避免与开启该
   * turn 的那条消息混为一谈。`id` 是服务端在 RPC 入口铸的 `interjection_id`。
   */
  | {
      type: "user_interjection";
      id: string;
      text: string;
      attachments?: Attachment[];
      images?: ImageRef[];
    }
  | { type: "agentMessage"; id: string; text: string }
  /**
   * 一段推理的展示条目（design §6.1 的「思考」卡片）。**不携带凭证**：signature /
   * encrypted_content 只存在于服务端回传 API 的路径上，绝不下发前端。
   *
   * 实时期间前端先用本地 `thread/reasoningDelta` 累积的块保留流式手感，本 item
   * 一到就**就地接管**（位置不变）；历史回放期则由本 item 直接驱动渲染。
   */
  | { type: "thinking"; id: string; text: string; redacted: boolean }
  | {
      type: "toolCall";
      id: string;
      call_id: string;
      name: string;
      input: unknown;
      status: ToolStatus;
      result?: string;
      images?: ImageRef[];
    };

export type TurnStatus = "completed" | "interrupted" | "failed";

/** thread 级实时状态；服务端权威（`thread/status/updated` / `thread/list(:All)`）。 */
export type ThreadStatus = "idle" | "running" | "awaiting_approval";

export type Notification =
  | {
      method: "thread/started";
      params: { thread_id: string; cwd: string; model: string; model_ref?: string | null };
    }
  | { method: "thread/status/updated"; params: { thread_id: string; status: ThreadStatus } }
  | { method: "thread/modelChanged"; params: { thread_id: string; model: string } }
  /**
   * 该会话的标题已变（LLM 首轮生成成功写入）。与 `thread/modelChanged` 同族：
   * 会话元数据，列表层恒推，所有客户端据此就地刷新侧栏。**直接携带权威标题**，
   * 客户端就地落笔，不必再拉一次 `thread/listAll`。
   */
  | { method: "thread/titleChanged"; params: { thread_id: string; title: string } }
  /**
   * 该会话的颜色标签已变。与 `thread/titleChanged` 同族：元数据变更、列表层恒推、
   * 直接携带权威值（`tag: null` = 已清除），客户端就地落笔。
   */
  | { method: "thread/tagChanged"; params: { thread_id: string; tag: string | null } }
  /**
   * 标签显示名（颜色 id → 名称）已变。**全局**帧，携带用户覆盖的权威映射
   * （可能只含被改的键），客户端与自己的默认名表合并后再渲染。
   */
  | { method: "ui/tagNames/updated"; params: { tag_names: Record<string, string> } }
  | {
      method: "thread/permissionModeChanged";
      params: { thread_id: string; mode: ThreadMode };
    }
  /**
   * 该会话的 reasoning effort 覆盖已切换到 `effort`（或清除）。与
   * `thread/modelChanged` 同族：会话元数据，列表层恒推，所有客户端的控件据此刷新。
   * `effort` **缺省即已清除覆盖**（不发送该字段），回落清单条目默认。
   */
  | {
      method: "thread/reasoningEffortChanged";
      params: { thread_id: string; effort?: string | null };
    }
  /**
   * 运行期告知（通道回退 / effort 降级），状态栏级一行提示。**注意 `level` 是
   * PascalCase**：core 的 `NoticeLevel` 无 `rename_all`，与同文件其余 snake_case
   * 参数不同（`protocol.rs:814-832` 钉死了这个线形状）。
   */
  | {
      method: "thread/notice";
      params: { thread_id: string; level: "Info" | "Warn"; text: string };
    }
  /**
   * 归一化的思维链增量（仅展示，**绝不回灌**后续轮次）。`kind` 同样 PascalCase
   * （core 的 `ReasoningKind` 无 `rename_all`）。
   */
  | {
      method: "thread/reasoningDelta";
      params: { thread_id: string; text: string; kind: "Content" | "Summary" };
    }
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
  /**
   * 该 thread 的某个受管后台进程状态变了。**不含 Output**：stdout 是高频流，
   * 只在用户打开详情时经 `process/read` 增量拉取。它只是「该重拉了」的提示，
   * 权威数据在 `process/list` 的响应里，收到后重拉即可。
   */
  | {
      method: "process/updated";
      params: { thread_id: string; process_id: string; state: string };
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
