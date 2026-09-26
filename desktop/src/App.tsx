import { useEffect, useRef, useState } from "react";
import { RpcClient } from "./lib/rpc";
import { Session } from "./lib/session";
import { tauriTransport } from "./tauriTransport";
import { ChatView } from "./components/ChatView";
import { MessageInput } from "./components/MessageInput";
import { StatusBar } from "./components/StatusBar";
import { ApprovalDialog } from "./components/ApprovalDialog";
import type { ApprovalRequest } from "./lib/protocol";

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
  const [threadId, setThreadId] = useState<string | null>(null);
  const [threadInfo, setThreadInfo] = useState<ThreadInfo | null>(null);
  const [approval, setApproval] = useState<ApprovalRequest | null>(null);
  const [status, setStatus] = useState<string>("connecting");

  useEffect(() => {
    if (inited.current) return; // guard against React StrictMode double-invoke
    inited.current = true;
    const client = new RpcClient(tauriTransport());
    clientRef.current = client;
    client.onNotification((n) => {
      session.apply(n);
      force((v) => v + 1);
    });
    client.onApproval((r) => setApproval(r));
    client.onStatus((s) => setStatus(s.state));
    (async () => {
      await client.request("initialize", {});
      const thread = await client.request<ThreadInfo & { thread_id: string }>("thread/start", {});
      setThreadId(thread.thread_id);
      setThreadInfo({ cwd: thread.cwd, model: thread.model });
      setStatus("connected");
    })().catch((e) => setStatus(`error: ${formatError(e)}`));
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
    if (threadId) void clientRef.current?.request("turn/interrupt", { threadId });
  };

  return (
    <>
      <div
        className="flex h-screen flex-col bg-neutral-950 text-neutral-100"
        inert={approval !== null}
      >
        <StatusBar
          cwd={threadInfo?.cwd ?? null}
          model={threadInfo?.model ?? null}
          status={status}
          usage={session.usage}
        />
        <ChatView items={session.items} error={session.lastError} />
        <MessageInput turnActive={session.turnActive} onSend={send} onInterrupt={interrupt} />
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
