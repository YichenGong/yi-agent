import { useEffect, useRef, useState } from "react";
import { RpcClient } from "./lib/rpc";
import { Session } from "./lib/session";
import { tauriTransport } from "./tauriTransport";
import { ChatView } from "./components/ChatView";
import { MessageInput } from "./components/MessageInput";
import { StatusBar } from "./components/StatusBar";
import { ApprovalDialog } from "./components/ApprovalDialog";
import { ThreadSidebar } from "./components/ThreadSidebar";
import type { ApprovalRequest, Workspace, WorkspaceGroup } from "./lib/protocol";
import { threadStartParams } from "./lib/threadStart";

interface ThreadInfo {
  cwd: string;
  model: string;
}

/**
 * RPC rejections are `RpcError` objects, so `String(e)` would render
 * `[object Object]`. Prefer the `message` field when present, falling back to
 * the default coercion for primitives and other shapes.
 */
function formatError(e: unknown): string {
  if (e && typeof e === "object" && "message" in e) {
    const m = (e as { message?: unknown }).message;
    if (typeof m === "string") return m;
  }
  return String(e);
}

export default function App() {
  const [session] = useState(() => new Session());
  const [, force] = useState(0);
  const clientRef = useRef<RpcClient | null>(null);
  const inited = useRef(false);
  const resuming = useRef(false);
  const [threadId, setThreadId] = useState<string | null>(null);
  const [threadInfo, setThreadInfo] = useState<ThreadInfo | null>(null);
  const [approval, setApproval] = useState<ApprovalRequest | null>(null);
  const [status, setStatus] = useState<string>("connecting");
  const [groups, setGroups] = useState<WorkspaceGroup[]>([]);
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);

  const busy = session.turnActive;

  const refreshThreads = async () => {
    const c = clientRef.current;
    if (!c) return;
    try {
      const r = await c.request<{ groups: WorkspaceGroup[] }>("thread/listAll", {});
      setGroups(r.groups);
    } catch {
      // 列表刷新失败不打断对话;下一次事件会再试。
    }
  };

  const refreshWorkspaces = async () => {
    const c = clientRef.current;
    if (!c) return;
    try {
      const r = await c.request<{ workspaces: Workspace[] }>("workspace/list", {});
      setWorkspaces(r.workspaces);
    } catch {
      // 最近目录刷新失败不影响当前对话。
    }
  };

  const resumeThread = async (threadId: string) => {
    const c = clientRef.current;
    if (!c || session.turnActive || resuming.current) return;
    resuming.current = true;
    // 必须同步 reset:回放通知可能先于 resume 响应到达。
    session.reset();
    force((v) => v + 1);
    try {
      const t = await c.request<ThreadInfo & { thread_id: string }>("thread/resume", {
        threadId,
      });
      setThreadId(t.thread_id);
      setThreadInfo({ cwd: t.cwd, model: t.model });
    } catch (e) {
      session.lastError = formatError(e);
      setThreadId(null);
      setThreadInfo(null);
      force((v) => v + 1);
    } finally {
      // 回放通知先于响应到达,故响应返回即代表本轮回放已全部应用;
      // 此时才允许下一次 resume,避免两个 thread 的历史交错合并。
      resuming.current = false;
    }
    await refreshThreads();
  };

  const newThread = async (cwd?: string) => {
    const c = clientRef.current;
    if (!c || session.turnActive || resuming.current) return;
    resuming.current = true;
    session.reset();
    force((v) => v + 1);
    try {
      const t = await c.request<ThreadInfo & { thread_id: string }>(
        "thread/start",
        threadStartParams(cwd),
      );
      setThreadId(t.thread_id);
      setThreadInfo({ cwd: t.cwd, model: t.model });
    } catch (e) {
      session.lastError = formatError(e);
      setThreadId(null);
      setThreadInfo(null);
      force((v) => v + 1);
    } finally {
      resuming.current = false;
    }
    await refreshThreads();
  };

  /** 打开原生文件夹选择器,返回选中的绝对路径(取消则 null)。 */
  const pickDirectory = async (): Promise<string | null> => {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({ directory: true, multiple: false });
    return typeof picked === "string" ? picked : null;
  };

  const addWorkspace = async (path: string): Promise<boolean> => {
    try {
      await clientRef.current?.request("workspace/add", { path });
      return true;
    } catch (e) {
      session.lastError = formatError(e);
      force((v) => v + 1);
      return false;
    }
  };

  /** 打开原生选择器 → 加入最近目录 → 在该目录新建对话。 */
  const onBrowse = async () => {
    if (session.turnActive || resuming.current) return;
    try {
      const dir = await pickDirectory();
      if (!dir) return;
      // add 失败(-32602 等)时不再建对话,错误已写入 lastError。
      if (!(await addWorkspace(dir))) return;
      await newThread(dir);
      await refreshWorkspaces();
    } catch (e) {
      session.lastError = formatError(e);
      force((v) => v + 1);
    }
  };

  /** 侧栏入口:带 cwd 直接在该目录新建;无 cwd 时退化为弹原生选择器。 */
  const onNew = async (cwd?: string) => {
    if (cwd) {
      await newThread(cwd);
      // thread/start 会顺带把该目录写入最近目录索引。
      await refreshWorkspaces();
    } else {
      await onBrowse();
    }
  };

  const removeWorkspace = async (path: string) => {
    try {
      await clientRef.current?.request("workspace/remove", { path });
    } catch (e) {
      session.lastError = formatError(e);
      force((v) => v + 1);
    }
    // 「移除」只是视图操作,不删数据:目录离开索引后 thread/listAll 不再扫描它,
    // 分组随之从侧栏消失,但 <dir>/.yi-agent/ 下的对话文件仍然保留。
    // 失败时也刷新,保证 UI 与服务端状态一致。
    await refreshThreads();
    await refreshWorkspaces();
  };

  const renameThread = async (id: string, title: string) => {
    const c = clientRef.current;
    if (!c || resuming.current) return;
    try {
      await c.request("thread/rename", { threadId: id, title });
    } catch (e) {
      session.lastError = formatError(e);
      force((v) => v + 1);
    }
    await refreshThreads();
  };

  const deleteThread = async (id: string) => {
    const c = clientRef.current;
    if (!c || session.turnActive || resuming.current) return;
    try {
      await c.request("thread/delete", { threadId: id });
    } catch (e) {
      session.lastError = formatError(e);
      force((v) => v + 1);
      return;
    }
    if (id === threadId) {
      session.reset();
      setThreadId(null);
      setThreadInfo(null);
      force((v) => v + 1);
    }
    await refreshThreads();
  };

  useEffect(() => {
    if (inited.current) return; // guard against React StrictMode double-invoke
    inited.current = true;
    const client = new RpcClient(tauriTransport());
    clientRef.current = client;
    client.onNotification((n) => {
      session.apply(n);
      force((v) => v + 1);
      if (n.method === "turn/completed") void refreshThreads();
    });
    client.onApproval((r) => setApproval(r));
    client.onStatus((s) => setStatus(s.state));
    (async () => {
      await client.request("initialize", {});
      await refreshWorkspaces();
      const list = await client.request<{ groups: WorkspaceGroup[] }>("thread/listAll", {});
      setGroups(list.groups);
      const first = list.groups.flatMap((g) => g.threads)[0];
      if (first) {
        await resumeThread(first.thread_id);
      }
      // 否则保持空态,等用户选目录新建(设计 §7.2:不再自动在 $HOME 建对话)。
      setStatus("connected");
    })().catch((e) => {
      const msg = formatError(e);
      session.lastError = msg;
      setStatus(`error: ${msg}`);
    });
  }, [session]);

  const send = async (text: string): Promise<boolean> => {
    if (!threadId || !clientRef.current) return false;
    session.addUserMessage(text);
    force((v) => v + 1);
    try {
      await clientRef.current.request("turn/start", {
        threadId,
        input: [{ type: "text", text }],
      });
      return true;
    } catch (e) {
      session.lastError = formatError(e);
      // Roll back the optimistic bubble so a rejected turn (e.g. -32012 turn
      // already in progress) does not leave a phantom user message.
      const last = session.items[session.items.length - 1];
      if (last && last.type === "userMessage" && last.text === text) session.items.pop();
      force((v) => v + 1);
      return false;
    }
  };

  const interrupt = () => {
    if (!threadId) return;
    clientRef.current?.request("turn/interrupt", { threadId }).catch((e) => {
      session.lastError = formatError(e);
      force((v) => v + 1);
    });
  };

  return (
    <>
      <div
        className="flex h-screen flex-row bg-neutral-950 text-neutral-100"
        inert={approval !== null}
      >
        <ThreadSidebar
          groups={groups}
          workspaces={workspaces}
          currentId={threadId}
          busy={busy}
          onSelect={resumeThread}
          onRename={renameThread}
          onDelete={deleteThread}
          onNew={onNew}
          onRemoveWorkspace={removeWorkspace}
          onBrowse={onBrowse}
        />
        <div className="flex min-w-0 flex-1 flex-col">
          <StatusBar
            cwd={threadInfo?.cwd ?? null}
            model={threadInfo?.model ?? null}
            status={status}
            usage={session.usage}
          />
          <ChatView
            items={session.items}
            error={session.lastError}
            retrying={session.retrying}
          />
          <MessageInput turnActive={session.turnActive} onSend={send} onInterrupt={interrupt} />
        </div>
      </div>
      {approval && (
        <ApprovalDialog
          key={approval.id}
          request={approval}
          onDecide={async (decision) => {
            try {
              await clientRef.current?.respond(approval.id, decision);
            } catch (e) {
              session.lastError = formatError(e);
              force((v) => v + 1);
            } finally {
              setApproval(null);
            }
          }}
        />
      )}
    </>
  );
}
