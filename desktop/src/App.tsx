import { useEffect, useRef, useState } from "react";
import { RpcClient } from "./lib/rpc";
import { Session } from "./lib/session";
import { tauriTransport } from "./tauriTransport";
import { ChatView } from "./components/ChatView";
import { MessageInput } from "./components/MessageInput";
import { StatusBar } from "./components/StatusBar";
import { ApprovalDialog } from "./components/ApprovalDialog";
import { ThreadSidebar } from "./components/ThreadSidebar";
import type { ApprovalRequest, ThreadSummary } from "./lib/protocol";

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
  const [threads, setThreads] = useState<ThreadSummary[]>([]);

  const busy = session.turnActive;

  const refreshThreads = async () => {
    const c = clientRef.current;
    if (!c) return;
    try {
      const r = await c.request<{ threads: ThreadSummary[] }>("thread/list", {});
      setThreads(r.threads);
    } catch {
      // 列表刷新失败不打断对话;下一次事件会再试。
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

  const newThread = async () => {
    const c = clientRef.current;
    if (!c || session.turnActive || resuming.current) return;
    resuming.current = true;
    session.reset();
    force((v) => v + 1);
    try {
      const t = await c.request<ThreadInfo & { thread_id: string }>("thread/start", {});
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

  const renameThread = async (id: string, title: string) => {
    const c = clientRef.current;
    if (!c || resuming.current) return;
    const prev = threads;
    setThreads((ts) => ts.map((t) => (t.thread_id === id ? { ...t, title } : t)));
    try {
      await c.request("thread/rename", { threadId: id, title });
    } catch (e) {
      setThreads(prev);
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
      const list = await client.request<{ threads: ThreadSummary[] }>("thread/list", {});
      setThreads(list.threads);
      if (list.threads.length > 0) {
        await resumeThread(list.threads[0].thread_id);
      } else {
        await newThread();
      }
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
          threads={threads}
          currentId={threadId}
          busy={busy}
          onSelect={resumeThread}
          onRename={renameThread}
          onDelete={deleteThread}
          onNew={newThread}
        />
        <div className="flex min-w-0 flex-1 flex-col">
          <StatusBar
            cwd={threadInfo?.cwd ?? null}
            model={threadInfo?.model ?? null}
            status={status}
            usage={session.usage}
          />
          <ChatView items={session.items} error={session.lastError} />
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
