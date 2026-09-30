import { useEffect, useRef, useState } from "react";
import { RpcClient } from "./lib/rpc";
import { ThreadStore } from "./lib/threadStore";
import { tauriTransport } from "./tauriTransport";
import { ChatView } from "./components/ChatView";
import { MessageInput } from "./components/MessageInput";
import { StatusBar } from "./components/StatusBar";
import { ApprovalDialog } from "./components/ApprovalDialog";
import { ApprovalBanner } from "./components/ApprovalBanner";
import { ThreadSidebar } from "./components/ThreadSidebar";
import type { ThreadStatus, TurnStatus, Workspace, WorkspaceGroup } from "./lib/protocol";
import { threadStartParams } from "./lib/threadStart";
import { setPermissionModeParams, type ThreadMode } from "./lib/threadPermissionMode";

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

/**
 * Read the persisted permission mode for a thread from the `thread/listAll`
 * groups. `thread/resume`/`thread/start` responses do not carry the mode, so it
 * is read back from the listing. Returns `null` when the thread is absent from
 * the listing (so callers never mistake "unknown" for "normal"); a present
 * thread with an omitted `permission_mode` (legacy) is treated as "normal".
 */
function modeForThread(groups: WorkspaceGroup[], id: string): ThreadMode | null {
  const t = groups.flatMap((g) => g.threads).find((th) => th.thread_id === id);
  return t ? (t.permission_mode ?? "normal") : null;
}

/**
 * `-32013` means `turn/interject` arrived with no turn running; `-32012` means
 * `turn/start` arrived while one was. Both say "the other method was the right
 * one" — the server resolves the disagreement, so the code is read from the raw
 * rejection (`formatError` would drop it).
 */
function sendMethodMismatchCode(e: unknown): number | null {
  const code = (e as { code?: unknown } | null)?.code;
  return code === -32013 || code === -32012 ? code : null;
}

export default function App() {
  // Per-thread state lives in a mutable store; the `force` tick is how React
  // learns that a view changed (the views are mutated in place, not setState'd).
  const storeRef = useRef(new ThreadStore());
  const store = storeRef.current;
  const [, force] = useState(0);
  const clientRef = useRef<RpcClient | null>(null);
  const inited = useRef(false);
  // Threads whose history has already been replayed (resumed) this session;
  // switching back to them must not replay again.
  const warm = useRef(new Set<string>());
  // Resume requests currently on the wire, so a rapid second click cannot
  // interleave two histories into one session.
  const inFlightResume = useRef(new Set<string>());
  // Approvals the user has dismissed from the banner; keyed by approval id.
  const dismissedApprovals = useRef(new Set<string>());
  const [currentId, setCurrentId] = useState<string | null>(null);
  const [status, setStatus] = useState<string>("connecting");
  const [groups, setGroups] = useState<WorkspaceGroup[]>([]);
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);

  const current = currentId ? store.view(currentId) : null;

  /** Write an error onto the current thread's session (if any). */
  const setCurrentError = (msg: string) => {
    const s = store.current()?.session;
    if (s) s.lastError = msg;
  };

  const refreshThreads = async (): Promise<WorkspaceGroup[] | null> => {
    const c = clientRef.current;
    if (!c) return null;
    try {
      const r = await c.request<{ groups: WorkspaceGroup[] }>("thread/listAll", {});
      setGroups(r.groups);
      store.seed(r.groups.flatMap((g) => g.threads));
      return r.groups;
    } catch {
      // 列表刷新失败不打断对话。返回 null 表示"未知":调用方必须把 null
      // 当作未知处理,不得回退成 normal。注意权限模式不会因此自动恢复——
      // 它保持未知,直到下一次切换线程时重新从 listAll 回读。
      return null;
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

  /**
   * Switch the visible thread. A warm thread (already resumed) only swaps the
   * view — its timeline kept accumulating in the background. A cold thread is
   * resumed once; per-thread isolation means the base session list/sidebar can
   * keep serving other threads in parallel.
   */
  const selectThread = async (id: string) => {
    store.select(id);
    setCurrentId(id);
    force((v) => v + 1);
    if (warm.current.has(id) || inFlightResume.current.has(id)) return; // warm → 只切视图
    const c = clientRef.current;
    if (!c) return;
    inFlightResume.current.add(id);
    try {
      await c.request("thread/resume", { threadId: id });
      // 删除竞态:resume 在途时用户可能已删掉该冷 thread,此时不能再把它
      // 加回 warm(会复活已删 id)。peek 不创建视图,仅判断是否仍存在。
      const view = store.peek(id);
      if (!view) return;
      warm.current.add(id);
      // thread/resume 响应不带权限模式,从 listAll 回读后写回该 view。
      // 按 thread 存储,切回 warm thread 时 chip 自动反映各自模式。
      const gs = await refreshThreads();
      if (gs !== null) view.mode = modeForThread(gs, id);
      force((v) => v + 1);
    } catch (e) {
      // 同样地,失败路径只在视图仍存在时写错误,避免 re-create 一个已被
      // drop 的 ThreadView(无界泄漏 + 残留 warm 条目)。
      const view = store.peek(id);
      if (view) view.session.lastError = formatError(e);
      force((v) => v + 1);
    } finally {
      inFlightResume.current.delete(id);
    }
  };

  const newThread = async (cwd?: string) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      const t = await c.request<{ thread_id: string; cwd: string; model: string }>(
        "thread/start",
        threadStartParams(cwd),
      );
      warm.current.add(t.thread_id);
      store.view(t.thread_id).info = { cwd: t.cwd, model: t.model };
      // 新对话默认 normal;仍从 listAll 回读以与服务端保持一致。
      store.view(t.thread_id).mode = "normal";
      store.select(t.thread_id);
      setCurrentId(t.thread_id);
      force((v) => v + 1);
      const gs = await refreshThreads();
      if (gs !== null) {
        store.view(t.thread_id).mode = modeForThread(gs, t.thread_id);
        force((v) => v + 1);
      }
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
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
      setCurrentError(formatError(e));
      force((v) => v + 1);
      return false;
    }
  };

  /** 打开原生选择器 → 加入最近目录 → 在该目录新建对话。 */
  const onBrowse = async () => {
    try {
      const dir = await pickDirectory();
      if (!dir) return;
      // add 失败(-32602 等)时不再建对话,错误已写入 lastError。
      if (!(await addWorkspace(dir))) return;
      await newThread(dir);
      await refreshWorkspaces();
    } catch (e) {
      setCurrentError(formatError(e));
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
      setCurrentError(formatError(e));
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
    if (!c) return;
    try {
      await c.request("thread/rename", { threadId: id, title });
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
    await refreshThreads();
  };

  const deleteThread = async (id: string) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      await c.request("thread/delete", { threadId: id });
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
      return;
    }
    // dismissedApprovals 以 approval id 为键;删除 thread 前先摘掉它名下那条
    // 审批,否则按 thread id delete 是 no-op 且会留下永不过期的条目。
    const v = store.peek(id);
    if (v?.approval) dismissedApprovals.current.delete(v.approval.id);
    store.drop(id);
    warm.current.delete(id);
    if (id === currentId) setCurrentId(null);
    force((v) => v + 1);
    await refreshThreads();
  };

  /** 切换当前 thread 的权限模式;仅当 RPC 成功后才更新本地状态。 */
  const setThreadMode = async (next: ThreadMode) => {
    const c = clientRef.current;
    const id = store.currentId;
    if (!c || !id) return;
    try {
      await c.request("thread/setPermissionMode", setPermissionModeParams(id, next));
      store.view(id).mode = next;
      force((v) => v + 1);
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
  };

  useEffect(() => {
    if (inited.current) return; // guard against React StrictMode double-invoke
    inited.current = true;
    const client = new RpcClient(tauriTransport());
    clientRef.current = client;
    client.onNotification((n) => {
      // 按 thread_id 路由:后台 thread 的流式输出照常累积,切回去即最新。
      store.applyNotification(n);
      force((v) => v + 1);
      if (n.method === "turn/completed") void refreshThreads();
    });
    client.onApproval((r) => {
      store.setApproval(r);
      force((v) => v + 1);
    });
    client.onStatus((s) => setStatus(s.state));
    (async () => {
      await client.request("initialize", {});
      await refreshWorkspaces();
      const list = await client.request<{ groups: WorkspaceGroup[] }>("thread/listAll", {});
      setGroups(list.groups);
      store.seed(list.groups.flatMap((g) => g.threads));
      const first = list.groups.flatMap((g) => g.threads)[0];
      if (first) {
        await selectThread(first.thread_id);
      }
      // 否则保持空态,等用户选目录新建(设计 §7.2:不再自动在 $HOME 建对话)。
      setStatus("connected");
    })().catch((e) => {
      const msg = formatError(e);
      setCurrentError(msg);
      setStatus(`error: ${msg}`);
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const send = async (text: string): Promise<boolean> => {
    const id = store.currentId;
    const c = clientRef.current;
    if (!id || !c) return false;
    const session = store.view(id).session;
    session.addUserMessage(text);
    force((v) => v + 1);
    // The cached status can be stale: `turn/completed` reaches us before the
    // server flips the thread back to idle, and a listing read inside that
    // window keeps the old value. Send the method the cache implies, then let
    // the server's error code say where it disagreed and switch to the other
    // one. Each direction is tried once, so two mismatches cannot ping-pong.
    const params = { threadId: id, input: [{ type: "text", text }] };
    let method: "turn/interject" | "turn/start" =
      store.peek(id)?.status === "running" ? "turn/interject" : "turn/start";
    let lastError: unknown = null;
    for (let hop = 0; hop < 2; hop += 1) {
      try {
        await c.request(method, params);
        return true;
      } catch (e) {
        lastError = e;
        const code = sendMethodMismatchCode(e);
        if (code === null) break;
        // Adopt the server's view so the next send (and the button) is right.
        method = method === "turn/interject" ? "turn/start" : "turn/interject";
        session.turnActive = method === "turn/interject";
        const view = store.peek(id);
        if (view) view.status = method === "turn/interject" ? "running" : "idle";
      }
    }
    session.lastError = formatError(lastError);
    // Roll back the optimistic bubble so a rejected send does not leave a
    // phantom user message.
    const last = session.items[session.items.length - 1];
    if (last && last.type === "userMessage" && last.text === text) session.items.pop();
    force((v) => v + 1);
    return false;
  };

  const interrupt = () => {
    const id = store.currentId;
    if (!id) return;
    clientRef.current?.request("turn/interrupt", { threadId: id }).catch((e) => {
      store.view(id).session.lastError = formatError(e);
      force((v) => v + 1);
    });
  };

  // Sidebar badges: server-authoritative status per thread + unread marker for
  // threads that finished a turn while the user was looking elsewhere.
  const statuses = new Map<string, ThreadStatus>();
  const unread = new Map<string, TurnStatus>();
  for (const g of groups)
    for (const t of g.threads) {
      const v = store.peek(t.thread_id);
      if (!v) continue;
      statuses.set(t.thread_id, v.status);
      if (v.unread && t.thread_id !== currentId)
        unread.set(t.thread_id, v.session.lastStatus ?? "completed");
    }

  const approval = current?.approval ?? null;
  const bannerItems = store
    .pendingApprovalsElsewhere()
    .filter((r) => !dismissedApprovals.current.has(r.id));

  return (
    <>
      <div className="flex h-screen flex-row bg-neutral-950 text-neutral-100">
        <ThreadSidebar
          groups={groups}
          workspaces={workspaces}
          currentId={currentId}
          statuses={statuses}
          unread={unread}
          onSelect={selectThread}
          onRename={renameThread}
          onDelete={deleteThread}
          onNew={onNew}
          onRemoveWorkspace={removeWorkspace}
          onBrowse={onBrowse}
        />
        <div className="relative flex min-w-0 flex-1 flex-col">
          <ApprovalBanner
            items={bannerItems}
            onJump={(id) => void selectThread(id)}
            onDismiss={() => {
              for (const r of bannerItems) dismissedApprovals.current.add(r.id);
              force((v) => v + 1);
            }}
          />
          <StatusBar
            cwd={current?.info?.cwd ?? null}
            model={current?.info?.model ?? null}
            status={status}
            usage={current?.session.usage ?? null}
          />
          <ChatView
            items={current?.session.items ?? []}
            error={current?.session.lastError ?? null}
            retrying={current?.session.retrying ?? null}
          />
          <MessageInput
            turnActive={current?.session.turnActive ?? false}
            onSend={send}
            onInterrupt={interrupt}
            mode={current?.mode ?? null}
            onModeChange={setThreadMode}
          />
          {approval && (
            <ApprovalDialog
              key={approval.id}
              request={approval}
              onDecide={async (decision) => {
                try {
                  await clientRef.current?.respond(approval.id, decision);
                } catch (e) {
                  const s = store.peek(approval.params.thread_id)?.session;
                  if (s) s.lastError = formatError(e);
                } finally {
                  store.clearApproval(approval.params.thread_id);
                  force((v) => v + 1);
                }
              }}
            />
          )}
        </div>
      </div>
    </>
  );
}
