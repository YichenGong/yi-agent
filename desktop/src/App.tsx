import { useCallback, useEffect, useRef, useState } from "react";
import { RpcClient } from "./lib/rpc";
import { ThreadStore } from "./lib/threadStore";
import { tauriTransport } from "./tauriTransport";
import { ChatView } from "./components/ChatView";
import { MessageInput } from "./components/MessageInput";
import { StatusBar } from "./components/StatusBar";
import { ApprovalDialog } from "./components/ApprovalDialog";
import { ApprovalBanner } from "./components/ApprovalBanner";
import { ThreadSidebar } from "./components/ThreadSidebar";
import { SubagentRail } from "./components/SubagentRail";
import { SubagentTrace } from "./components/SubagentTrace";
import { TitleBar } from "./components/TitleBar";
import type {
  AgentCancelPreviewResult,
  AgentChildrenListResult,
  AgentTraceSnapshotResult,
  ThreadStatus,
  TurnStatus,
  Workspace,
  WorkspaceGroup,
} from "./lib/protocol";
import { childrenOf, SubagentRailStore } from "./lib/subagents";
import { formatError } from "./lib/errorMessage";
import { SuperpowersKanbanView } from "./components/SuperpowersKanbanView";
import { SuperpowersKanbanSettings } from "./components/SuperpowersKanbanSettings";
import { SuperpowersKanbanCollapsedStrip } from "./components/SuperpowersKanbanCollapsedStrip";
import {
  type BoardCardDto,
  type SwitchSource,
  fetchBoard,
  readBoardSwitch,
  setBoardSwitch,
} from "./lib/superpowersKanbanSwitch";
import { threadStartParams } from "./lib/threadStart";
import { setPermissionModeParams, type ThreadMode } from "./lib/threadPermissionMode";
import { renderHelp } from "./lib/slash";
import { estimateCost, formatCost } from "./lib/pricing";

/**
 * RPC rejections are `RpcError` objects, so `String(e)` would render
 * `[object Object]`. Prefer the `message` field when present, falling back to
 * the default coercion for primitives and other shapes.
 */
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
  // 子 agent 暂留区:按对话隔离的折叠状态 + 用户是否收起了它。收起是纯 UI 选择,
  // 不进 store;数据本身仍随通知累积,展开即是当前值。
  const railStore = useRef(new SubagentRailStore()).current;
  const [railCollapsed, setRailCollapsed] = useState(false);
  // 打开的详情:进入栈 + 该栈顶任务的轨迹行。栈让"子任务再进入"可退回,
  // 与 TUI 页签同一语义。
  const [detailStack, setDetailStack] = useState<string[]>([]);
  const [traceRows, setTraceRows] = useState<AgentTraceSnapshotResult["rows"]>([]);
  const openSubagent = detailStack.length > 0 ? detailStack[detailStack.length - 1] : null;
  // 通知回调在 effect 里注册一次,读不到最新的 state,所以当前打开的任务放在 ref。
  const openTaskId = useRef<string | null>(null);
  useEffect(() => {
    openTaskId.current = openSubagent;
  }, [openSubagent]);
  const [boardOn, setBoardOn] = useState(false);
  const [boardSource, setBoardSource] = useState<SwitchSource>("default");
  const [boardCards, setBoardCards] = useState<BoardCardDto[]>([]);
  // Collapsed board panel. Deliberately not persisted: every launch starts
  // expanded, and the switch state is independent of the panel being folded.
  const [kanbanCollapsed, setKanbanCollapsed] = useState(false);

  /** 经 app-server 调 board RPC；未连接时直接失败。 */
  const boardRpc = useCallback(
    <T = unknown,>(method: string, params: unknown): Promise<T> => {
      const c = clientRef.current;
      if (!c) return Promise.reject(new Error("not connected"));
      return c.request<T>(method, params);
    },
    [],
  );

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
   * 打开某任务的详情:回灌历史,然后开一条流。
   *
   * 关闭(或换任务)时先 unwatch 再开新的,任何时刻只有一路流:否则换任务后
   * 旧任务的 `agent/trace/event` 仍会到达,把两个任务的轨迹混进一个视图。
   */
  const openDetail = async (threadId: string, taskId: string) => {
    const c = clientRef.current;
    if (!c) return;
    setDetailStack([taskId]);
    setTraceRows([]);
    try {
      const snapshot = await c.request<AgentTraceSnapshotResult>("agent/trace/read", {
        threadId,
        taskId,
      });
      setTraceRows(snapshot.rows);
      await c.request("agent/trace/watch", { threadId, taskId });
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
  };

  const closeDetail = async (threadId: string) => {
    setDetailStack([]);
    setTraceRows([]);
    try {
      await clientRef.current?.request("agent/trace/unwatch", { threadId });
    } catch {
      // 关闭是本地动作:即使 unwatch 失败也不把详情留在屏幕上。
    }
  };

  /**
   * 取某对话当前的子 agent 列表。
   *
   * 失败即放弃:暂留区是附属视图,列表读不到就保持上一次的值(或空),绝不因为
   * 它把对话打断。daemon 未 attach 时服务端返回空表,这里不会走到 catch。
   */
  const refreshSubagents = async (threadId: string) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      const r = await c.request<AgentChildrenListResult>("agent/children/list", {
        threadId,
      });
      railStore.set(threadId, r.children);
      force((v) => v + 1);
    } catch {
      // 附属视图:读失败不改动已有内容。
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
    // 该对话的子 agent 列表:重进对话时重新拉取,免得依赖"通知一定到过"。
    void refreshSubagents(id);
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
      // 子 agent 的通知不进 ThreadStore:它们是对话的附属视图,不是对话本身。
      if (n.method === "agent/children/updated") {
        railStore.applyNotification(n.params.threadId, n.params.children);
        force((v) => v + 1);
        return;
      }
      if (n.method === "agent/trace/event") {
        // 只接受当前打开任务的流:换任务时旧流可能还有在途帧,丢弃它们比
        // 把两个任务的轨迹拼在一起安全。
        setTraceRows((prev) =>
          n.params.taskId === openTaskId.current ? [...prev, n.params.row] : prev,
        );
        return;
      }
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
    // 看板轮询：读取失败绝不影响主流程。
    const refreshBoard = async () => {
      try {
        const [sw, cards] = await Promise.all([
          readBoardSwitch(boardRpc),
          fetchBoard(boardRpc),
        ]);
        setBoardOn(sw.on);
        setBoardSource(sw.source);
        setBoardCards(cards);
      } catch {
        /* 看板读取失败绝不影响主流程 */
      }
    };
    void refreshBoard();
    const boardTimer = window.setInterval(refreshBoard, 2000);
    return () => window.clearInterval(boardTimer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /**
   * Run a slash command. Commands never reach the agent: their output is a
   * `notice` on the current session, mirroring the TUI's Separator cells.
   */
  const onSlashCommand = async (name: string, args: string | null) => {
    const id = store.currentId;
    const c = clientRef.current;
    if (!id || !c) return;
    const view = store.view(id);
    const session = view.session;
    switch (name) {
      case "help":
        session.notice(renderHelp(args));
        break;
      case "cost": {
        const u = session.usage;
        if (!u) {
          session.notice("暂无用量数据");
          break;
        }
        const cost = estimateCost(u);
        session.notice(
          [
            `model: ${u.model}`,
            `input: ${u.input}  output: ${u.output}`,
            `cache read: ${u.cacheRead}  cache write: ${u.cacheWrite}`,
            `估算成本: ${formatCost(cost)}`,
          ].join("\n"),
        );
        break;
      }
      case "model":
        session.notice(`当前模型: ${view.info?.model ?? "未知"}`);
        break;
      case "config":
        try {
          const cfg = await c.request<Record<string, unknown>>("config/read", {});
          session.notice(
            Object.entries(cfg)
              .map(([k, v]) => `${k}: ${String(v)}`)
              .join("\n"),
          );
        } catch (e) {
          session.notice(`读取配置失败: ${formatError(e)}`);
        }
        break;
      case "clear":
        try {
          await c.request("thread/clear", { threadId: id });
          session.reset();
          session.notice("对话已清空");
        } catch (e) {
          // 失败绝不清界面:否则会出现"看着清了、服务端还记得"的假象。
          session.notice(`清空失败: ${formatError(e)}`);
        }
        break;
      case "compact":
        try {
          const r = await c.request<{ status?: string; error?: string }>("thread/compact", {
            threadId: id,
          });
          if (r.status === "compacted") session.notice("对话已压缩");
          else if (r.status === "not_reduced") session.notice("无需压缩：历史太短");
          else session.notice(`压缩失败: ${r.error ?? "未知结果"}`);
        } catch (e) {
          session.notice(`压缩失败: ${formatError(e)}`);
        }
        break;
      default:
        session.notice(`未知命令: /${name}\n${renderHelp()}`);
        break;
    }
    force((v) => v + 1);
  };

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
      <div className="flex h-screen flex-col bg-neutral-950 text-neutral-100">
        <TitleBar />
        <div className="flex min-h-0 flex-1 flex-row">
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
        {kanbanCollapsed ? (
          <SuperpowersKanbanCollapsedStrip onExpand={() => setKanbanCollapsed(false)} />
        ) : (
          <div className="flex w-72 flex-col border-r border-neutral-800">
            <SuperpowersKanbanSettings
              switchOn={boardOn}
              source={boardSource}
              onToggle={(next) => {
                void setBoardSwitch(boardRpc, next)
                  .then(() => {
                    setBoardOn(next);
                    setBoardSource("project");
                  })
                  .catch(() => {
                    /* 写失败保持原状，下一轮轮询会纠正 */
                  });
              }}
              onCollapse={() => setKanbanCollapsed(true)}
            />
            <SuperpowersKanbanView switchOn={boardOn} source={boardSource} cards={boardCards} />
          </div>
        )}
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
            onSlashCommand={(name, args) => void onSlashCommand(name, args)}
          />
          {currentId && openSubagent && (
            <SubagentTrace
              taskId={openSubagent}
              row={
                railStore.get(currentId).find((r) => r.taskId === openSubagent) ?? null
              }
              children={childrenOf(railStore.get(currentId), openSubagent)}
              rows={traceRows}
              onClose={() => void closeDetail(currentId)}
              onDrill={(taskId) => void openDetail(currentId, taskId)}
              onMessage={async (taskId, message) => {
                await clientRef.current?.request("agent/message", {
                  threadId: currentId,
                  taskId,
                  message,
                });
              }}
              onCancel={async (taskId) =>
                await clientRef.current!.request<AgentCancelPreviewResult>(
                  "agent/cancel/preview",
                  { threadId: currentId, taskId },
                )
              }
              onConfirmCancel={async (taskId, token) => {
                await clientRef.current?.request("agent/cancel", {
                  threadId: currentId,
                  taskId,
                  confirmationToken: token,
                });
              }}
            />
          )}
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
        {currentId && !railCollapsed && (
          <SubagentRail
            rows={railStore.get(currentId)}
            selectedTaskId={openSubagent}
            onOpen={(taskId) => void openDetail(currentId, taskId)}
            onCollapse={() => setRailCollapsed(true)}
          />
        )}
        </div>
      </div>
    </>
  );
}
