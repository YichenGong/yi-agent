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
    })().catch((e) => setStatus(`error: ${String(e)}`));
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
      session.lastError = String(e);
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
    <div className="flex h-screen flex-col bg-neutral-950 text-neutral-100">
      <StatusBar
        cwd={threadInfo?.cwd ?? null}
        model={threadInfo?.model ?? null}
        status={status}
        usage={session.usage}
      />
      <ChatView items={session.items} error={session.lastError} />
      <MessageInput turnActive={session.turnActive} onSend={send} onInterrupt={interrupt} />
      {approval && (
        <ApprovalDialog
          request={approval}
          onDecide={async (decision) => {
            await clientRef.current?.respond(approval.id, decision);
            setApproval(null);
          }}
        />
      )}
    </div>
  );
}
