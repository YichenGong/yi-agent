import { useRef, useState } from "react";
import { formatError } from "../lib/errorMessage";
import { type DiffTextResult, type ThreadDiffResult } from "../lib/protocol";
import type { ViewFrame } from "../lib/viewFrame";
import type { RpcClient } from "../lib/rpc";

/**
 * 会话级 Git Diff 面板的状态与动作（右侧详情栏的 diff Tab 消费）。
 *
 * 归属是这里唯一的硬约束：快照、错误、下钻态、提示语/基准都必须答得出「这是
 * **哪条**会话的 diff」。判据 `currentIdRef` 由调用方注入——它是「此刻选中谁」的
 * 唯一事实来源，`App.tsx` 的生命周期闭包（`ui/gitDiff/focus` 的通知处理）读的是
 * 同一个 ref，故不在这里另起一份影子。
 *
 * `rpc` 是「取客户端」的 getter：未连接时返回 null，动作**静默返回**。这条语义
 * 逐字保留自搬移前（原为 `const c = clientRef.current; if (!c) return;`）：diff 是
 * 附属视图，断线读不到不该在对话里留一条红字，更不该漏出一个没人 catch 的
 * rejection（故不学 `pluginCall` 那套 `Promise.reject` 接缝）。
 *
 * 面板的开合（`panelOpen`）不属于 diff：它是会话关注点的状态，由 `App.tsx` 在
 * 调用 `resetThreadDiff` 的现场自行收起（见那几处调用点）。
 */
export function useDiffState({
  rpc,
  currentIdRef,
}: {
  /** 取当前的 RPC 客户端；未连接即 null（各动作据此静默返回）。 */
  rpc: () => RpcClient | null;
  /** 当前会话 id 的 live 影子（App 维护，与本 hook 只读共享）。 */
  currentIdRef: { current: string | null };
}) {
  // 会话级 diff 快照与它的加载态/提交下钻态，全部由面板消费。
  const [threadDiff, setThreadDiff] = useState<ThreadDiffResult | null>(null);
  const [diffLoading, setDiffLoading] = useState(false);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [activeCommit, setActiveCommit] = useState<{ sha: string; text: string } | null>(null);
  // diff 是**有主**的：`threadDiff` / `diffError` / `activeCommit` 都必须配上它们
  // 属于哪条会话。只为「当前会话」写这些槽还不够——`thread/diff/read` 在途时用户
  // 可能已经切走，迟到的响应会把别的会话的 diff 画进当前视图。故用两个 ref 兜底：
  // `diffBase` / `diffNote`（提示语）与快照同属一次推送/一次读取，存成一条记录整体
  // 替换，读的时候靠 `diffOwnerRef` 认领，不是当前会话的概不渲染。
  const [diffBase, setDiffBase] = useState<string | null>(null);
  const [diffNote, setDiffNote] = useState<string | null>(null);
  const diffOwnerRef = useRef<string | null>(null);
  // 自增的请求号：只有**最新**那次 `thread/diff/read` 有资格落盘。会话切换不改动它，
  // 所以「A 的在途响应在切换到 B 之后才回来」会被号数 + 归属两道门同时挡掉。
  const diffReqRef = useRef(0);
  // 模型给的视图帧（`thread/view/read`）：**独立**于上面那份结构化快照，有自己的
  // 请求号与归属判据（见 `loadViewFrame`）。
  const [viewFrame, setViewFrame] = useState<ViewFrame | null>(null);
  const viewReqRef = useRef(0);
  const viewOwnerRef = useRef<string | null>(null);
  // 推送随行提示语（`ui/gitDiff/focus` 的 `note`）的挂起值：提示语解释的是某条会话
  // 的这一份 diff，不能先于快照落到屏幕上（旧快照 + 新提示语正是一次静默错配），
  // 故不直接进 state，等读取认领成功后再由 `loadThreadDiff` 摘取。
  const pendingNoteRef = useRef<string | null>(null);

  /**
   * 读取**指定会话**的 Git Diff 快照（工作区 + 与默认分支的差异）。
   *
   * 失败即把错误放进面板自己的错误槽，不写会话的 `lastError`：diff 是附属视图，
   * 读不到不该在对话里留一条红字。`base` 有值即带基准（模型推送时可指定）。
   *
   * 两道防串门：认领本次读取（`diffReqRef` 自增，只有最新一次能落盘），以及
   * 落盘前再确认目标仍是当前会话（`threadId === currentIdRef.current`）。没有
   * 后者，切走之后迟到的响应仍会把别的会话的 diff 画进当前视图；没有前者，
   * 同会话内先发的慢响应会盖掉后发的快响应。两者都不成立时连 loading 一起丢，
   * 免得留下一个永远转不完的加载态。
   */
  const loadThreadDiff = async (threadId: string, base?: string | null) => {
    const c = rpc();
    if (!c) return;
    diffReqRef.current += 1;
    const token = diffReqRef.current;
    const owned = () => token === diffReqRef.current && threadId === currentIdRef.current;
    setDiffLoading(true);
    setDiffError(null);
    setActiveCommit(null);
    // 这次读取认领快照：提示语/基准只在它回来时才落，且必须配同一条会话。
    diffOwnerRef.current = threadId;
    try {
      const r = await c.request<ThreadDiffResult>("thread/diff/read", {
        threadId,
        ...(base ? { base } : {}),
      });
      if (!owned()) return;
      setThreadDiff(r);
      setDiffNote(pendingNoteRef.current);
      setDiffBase(base ?? null);
    } catch (e) {
      if (!owned()) return;
      setDiffError(formatError(e));
    } finally {
      if (owned()) setDiffLoading(false);
    }
  };

  /**
   * 设置推送随行提示语的挂起值（见 `pendingNoteRef`）。
   *
   * `null` 同样要走这一步：切会话/无提示的聚焦必须能清掉上一条提示，否则它会
   * 一直挂在下一份快照上。
   */
  const setPendingDiffNote = (note: string | null) => {
    pendingNoteRef.current = note;
    setDiffNote(null);
  };

  /**
   * 下钻到单个 commit：复用 `thread/diff/read` 的 `commit` 分支拿它的 unified diff。
   *
   * 与 `loadThreadDiff` 分开：这里不清空 diff（返回按钮要能退回全部改动），也不动
   * loading（面板已有内容，闪一下"正在计算"反而跳）。仍受归属约束：只有当前会话
   * 的 commit 能落实，换了会话的在途响应一律丢弃（否则会顶着一个空壳"返回"视图）。
   */
  const openCommit = async (threadId: string, sha: string) => {
    const c = rpc();
    if (!c) return;
    try {
      const r = await c.request<DiffTextResult>("thread/diff/read", { threadId, commit: sha });
      if (threadId !== currentIdRef.current) return;
      setActiveCommit({ sha, text: r.unifiedDiff });
    } catch (e) {
      if (threadId !== currentIdRef.current) return;
      setDiffError(formatError(e));
    }
  };

  /**
   * 读取**指定会话**的模型视图帧（`thread/view/read`）。
   *
   * 帧是「模型给的一条命令的输出」（或结构化默认），与 `threadDiff` 是**两份**快照：
   * 面板优先渲染帧，无帧才退回结构化 diff（`GitDiffView`）。因此这里有自己的一套
   * 归属门（`viewReqRef` + `viewOwnerRef`），与 diff 的那套互不干扰——两条请求会
   * 并发在途（focus 两条都发），任一方迟到都不得画进别的会话。
   *
   * 与 `loadThreadDiff` 的两点不同：
   * - **不清结构化快照**（`threadDiff`）。帧覆盖的是正文，无帧时面板仍要那张快照
   *   兜底；清掉它会在帧到达前后闪一次「尚无 diff 数据」。
   * - **失败静默**：帧服务端没有就是 `null`（非错误），断线读不到也不该在对话里留
   *   红字——与 diff 一致，故不写 `diffError`。
   *
   * `refresh` 为真时请服务端重跑帧里的命令（或重算结构化默认）。
   */
  const loadViewFrame = async (threadId: string, refresh = false) => {
    const c = rpc();
    if (!c) return;
    viewReqRef.current += 1;
    const token = viewReqRef.current;
    const owned = () => token === viewReqRef.current && threadId === currentIdRef.current;
    viewOwnerRef.current = threadId;
    try {
      const r = await c.request<ViewFrame | null>("thread/view/read", {
        threadId,
        ...(refresh ? { refresh: true } : {}),
      });
      if (!owned()) return;
      setViewFrame(r ?? null);
    } catch {
      // 静默：帧是附属视图，读不到就退回结构化快照，不留红字（同 `loadThreadDiff`
      // 断线静默的理由）。
    }
  };

  /**
   * 单文件正文懒加载（D1）：文件被截断、正文缺席时，为该 path 取一次正文。
   *
   * 复用 `thread/diff/read` 的 `path` 分支（既有 RPC，未删）。取到就把它**并进**
   * 当前结构化快照的 `unifiedDiff`：`GitDiffView` 从快照的 `unifiedDiff` 解析正文，
   * 不并进去等于白取。归属仍按当前会话判：切走了就丢。
   */
  const loadFile = async (threadId: string, path: string) => {
    const c = rpc();
    if (!c) return;
    try {
      const r = await c.request<DiffTextResult>("thread/diff/read", { threadId, path });
      if (threadId !== currentIdRef.current) return;
      setThreadDiff((prev) => {
        // 只并进**本会话**的快照；没有快照（还没有结构化数据）时无从并入。
        if (!prev || diffOwnerRef.current !== threadId) return prev;
        if (r.unifiedDiff === "") return prev;
        return { ...prev, unifiedDiff: `${prev.unifiedDiff}\n${r.unifiedDiff}` };
      });
    } catch (e) {
      if (threadId !== currentIdRef.current) return;
      setDiffError(formatError(e));
    }
  };

  /**
   * 丢掉会话级 diff 的全部状态。
   *
   * diff 是**有主**的（属于某条会话），凡是「当前会话变了」或「会话没了」都必须
   * 走这里，否则新会话会先闪一眼旧 diff。清的不只是快照本身：归属、提示语/基准、
   * 下钻态、错误一起清——它们都是那一次读取/推送的一部分，留下任何一件都会和
   * 新会话的快照拼成一张错页。会话切换**不**动 `diffReqRef`：在途响应归号数为准，
   * 切走即失效，但下一次读取仍能拿到更大的号。
   */
  const resetThreadDiff = () => {
    setThreadDiff(null);
    setActiveCommit(null);
    setDiffError(null);
    setDiffBase(null);
    setDiffNote(null);
    // loading 同样属于那条会话：留着它，下一次打开面板会先闪一句「正在计算」
    // （在途响应的 finally 因归属已失效而不再替我们清）。
    setDiffLoading(false);
    diffOwnerRef.current = null;
    pendingNoteRef.current = null;
    // 帧与结构化快照是两份快照，一起复位：只清一份会让新会话先闪一眼旧帧
    // （或旧快照）——不过期的视图冒充当前会话，正是这里的头号风险。
    setViewFrame(null);
    viewOwnerRef.current = null;
  };

  return {
    // state：面板直接消费的七项。
    threadDiff,
    diffLoading,
    diffError,
    activeCommit,
    diffBase,
    diffNote,
    viewFrame,
    // actions。
    setActiveCommit,
    loadThreadDiff,
    loadViewFrame,
    loadFile,
    setPendingDiffNote,
    openCommit,
    resetThreadDiff,
    // refs：App 的 JSX 与生命周期 effect 直接读（归属判据与在途请求号）。
    diffOwnerRef,
    diffReqRef,
    viewOwnerRef,
    viewReqRef,
    pendingNoteRef,
  };
}
