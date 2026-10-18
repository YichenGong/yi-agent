import { useCallback, useEffect, useRef, useState, type RefObject } from "react";
import { type BoardErrorKind, BOARD_SUMMARY_UNREADABLE, summarize } from "../lib/boardIndex";
import { makeBoardTick } from "../lib/boardRefresh";
import { listBoards } from "../lib/superpowersKanbanBoards";
import { normalizeCard, type BoardCard } from "../lib/superpowersKanbanState";
import {
  type SwitchSource,
  fetchBoard,
  pluginIsUnavailable,
  readBoardSwitch,
} from "../lib/superpowersKanbanSwitch";
import type { RpcClient } from "../lib/rpc";

/**
 * 看板关注点：主区域的看板页、侧栏的看板登记表与摘要，以及宿主级「后台值守」
 * 开关（`SuperpowersKanbanView`/`SuperpowersKanbanSettings` 消费）。
 *
 * 归属是这里唯一的硬约束，且**分两层**。卡片、开关（`boardOn`/`boardSource`）、
 * 失败的说法（`boardError`）都必须答得出「这是**哪个项目**的看板」，判据是
 * `selectedBoard`——它由本 hook 自己持有：「看板页在看哪个项目」本身就是这套状态
 * 的一部分（打开看板只改它、不动 `currentId`），另起一份影子只会两处打架。而值守
 * （`watchmanEnabled`/`watchmanWarning`）管的是宿主的 daemon，与选中的项目无关，
 * 故不随切项目重置。
 *
 * `clientRef` 是 App 的 RPC 客户端槽，`boardRpc` 由它在本 hook 内构出并对外返回。
 * 这条接缝**未连接时直接失败**（`Promise.reject(new Error("not connected"))`），
 * 语义逐字保留自搬移前：看板是主区域的平级页面，写入失败必须报出来（`boardError`
 * 与 `onCreateBoard` 那几处），不能像附属视图那样静默——故不学 `useDiffState` 的
 * `() => RpcClient | null` 接缝。空依赖即可稳定：它只闭合一个 ref。`App.tsx` 的
 * JSX（`enqueueBoardCard`）与它那几个动作都要用，所以必须返回出去。
 *
 * 跨关注点的编排留在 App（R6'）：`selectThread`/`newThread` 里的
 * `setSelectedBoard(null)`（离开看板）、`onCreateBoard`/`onRemoveBoard`/
 * `onOpenBoard`/`onToggleBoardSwitch`/`onToggleWatchman`（它们要写会话的
 * `setCurrentError` 与 `force`），以及握手后拉登记表、注册 2 秒计时器那两个现场。
 * 本 hook 因此把 `boardRpc`/`refreshBoards`/`setBoardError`/`setSelectedBoard` 等
 * 返回出去，供那些现场使用。`boardSeq` 只服务于这里的读取，故不外露。
 */
export function useBoardState({
  clientRef,
}: {
  /** App 的 RPC 客户端槽：`boardRpc` 从这里取客户端（只闭合它，故空依赖即稳定）。 */
  clientRef: RefObject<RpcClient | null>;
}) {
  const [boardOn, setBoardOn] = useState(false);
  const [boardSource, setBoardSource] = useState<SwitchSource>("default");
  const [boardCards, setBoardCards] = useState<BoardCard[]>([]);
  // The plugin answers every board question, so an unanswered query means it is
  // not installed. Distinct from "installed and empty".
  const [boardPluginMissing, setBoardPluginMissing] = useState(false);
  // 上一次看板写入的失败：非空即把「为什么没生效」摆在面板上，而不是静默。
  // 只有下一次写入成功或换了项目才清掉——轮询成功不算，那会让一条刚报出的
  // 失败在 2 秒内自己消失。
  const [boardError, setBoardError] = useState<BoardErrorKind | null>(null);
  // 宿主级「后台值守」：默认开（老配置没有这个键时也当作开，否则会把用户本来
  // 一直在跑的值守悄悄关掉）。值守管的是宿主的 daemon，与选中的项目无关。
  const [watchmanEnabled, setWatchmanEnabled] = useState(true);
  // 上一次值守写入携带的告知（安装/卸载失败原因）。每次写入都以返回值整体覆盖，
  // 成功时清空——不做乐观更新，那是宿主的状态，得听宿主怎么说。
  const [watchmanWarning, setWatchmanWarning] = useState<string | null>(null);

  /**
   * 一行看板状态，2 秒刷新一次。
   *
   * 用 ref 指向「最近一次的刷新函数」：副作用只在挂载时注册一次计时器，换项目
   * 时换掉里面调的闭包，而不是重建计时器（重建会漏掉切换瞬间的竞态，也会让
   * 两个项目的响应互相覆盖）。
   */
  const boardTick = useRef<() => void>(() => {});
  /** 看板读取的序号，用来丢掉换项目后落地的过期响应。 */
  const boardSeq = useRef(0);

  // 值守同理：一旦用户拨过开关（onToggleWatchman 会写回宿主），后续任何一趟
  // 在途的 read——尤其是重启后的重放握手——都不得用旧值把它覆盖回去。与主题
  // 不同，值守没有 ui/settings/updated 通知，所以触碰后只认写入的返回值。
  const watchmanTouchedRef = useRef(false);

  /** 经 app-server 调 board RPC；未连接时直接失败。 */
  const boardRpc = useCallback(
    <T = unknown,>(method: string, params: unknown): Promise<T> => {
      const c = clientRef.current;
      if (!c) return Promise.reject(new Error("not connected"));
      return c.request<T>(method, params);
    },
    [],
  );

  // 看板 RPC 按项目问话（`project` 进 plugin/query 的参数）。当前在主区域
  // 展示看板的项目；看板不切换会话，但点某条会话会离开看板（见 selectThread）。
  const [selectedBoard, setSelectedBoard] = useState<string | null>(null);
  // done 列是否展开。纯界面状态：每次打开/切换看板都重置为折叠。
  const [doneExpanded, setDoneExpanded] = useState(false);
  // 登记了看板的项目（侧栏据此画条目 + 决定右键菜单给创建还是移除）。
  const [boards, setBoards] = useState<string[]>([]);
  // 项目路径 → 摘要。挂在侧栏条目上，扫一眼就知道各项目积压多少。
  const [boardSummaries, setBoardSummaries] = useState<Record<string, string>>({});

  /**
   * 拉一次「哪些项目有看板」以及各项目的摘要。
   *
   * 摘要逐个项目单独读、单独失败：一个项目的 daemon 没起来不该让整张表消失
   * （条目照画，摘要留空）。整张表读不到才当作没有看板——侧栏据此不画条目。
   */
  const refreshBoards = useCallback(async () => {
    try {
      const list = await listBoards(boardRpc);
      setBoards(list);
      const entries = await Promise.all(
        list.map(async (path): Promise<[string, string]> => {
          try {
            return [path, summarize(await fetchBoard(boardRpc, path))];
          } catch {
            // 摘要读不到就明说「无法读取」，别留空：留空和「空看板」长得一样，
            // 而这两件事对用户是两码事（一个要去看 daemon/插件，一个什么都不用做）。
            return [path, BOARD_SUMMARY_UNREADABLE];
          }
        }),
      );
      setBoardSummaries(Object.fromEntries(entries));
    } catch {
      setBoards([]);
      setBoardSummaries({});
    }
  }, [boardRpc]);

  /**
   * 读选中项目的看板。没选中项目就什么都不问，也把面板清空——否则切走之后
   * 旧项目的卡片会留在屏幕上，看着像新项目的。
   *
   * 读取失败不改主流程，也不报错：失败在这里是常态（daemon 没起、插件没装），
   * 真正需要用户知道的失败是**写入**失败。
   */
  const refreshSelectedBoard = useCallback(async () => {
    if (selectedBoard === null) {
      setBoardCards([]);
      setBoardPluginMissing(false);
      return;
    }
    // 每次读领一个号。切项目后旧项目那次读可能还在路上，落地时必须认出自己
    // 已经过期——否则 A 的卡片会盖在 B 的看板上，看着像 B 的队列。
    const seq = ++boardSeq.current;
    try {
      const [sw, cards] = await Promise.all([
        readBoardSwitch(boardRpc, selectedBoard),
        fetchBoard(boardRpc, selectedBoard),
      ]);
      if (seq !== boardSeq.current) return;
      setBoardOn(sw.on);
      setBoardSource(sw.source);
      setBoardCards(cards.map(normalizeCard).filter((c): c is BoardCard => c !== null));
      setBoardPluginMissing(false);
    } catch (error) {
      if (seq !== boardSeq.current) return;
      // 插件不在就明说：那意味着看板根本没有后端，而不是「装好了但没有卡片」。
      if (pluginIsUnavailable(error)) setBoardPluginMissing(true);
    }
  }, [boardRpc, selectedBoard]);

  useEffect(() => {
    // 计时器只建一次，每次响都走「当前项目」的读法：重建计时器会漏掉切换
    // 瞬间在途的那一拍。
    // 一拍要同时刷选中看板与侧栏摘要：摘要若只在握手时读一次，侧栏会一直
    // 停在旧值，看起来像「什么都没在跑」。
    boardTick.current = makeBoardTick(refreshSelectedBoard, refreshBoards);
    // 选中项目一落地就读一次，不等下一拍轮询：否则点开看板会有最多 2 秒的空面板。
    void refreshSelectedBoard();
  }, [refreshSelectedBoard, refreshBoards]);

  return {
    // state：看板页与侧栏直接消费的十一项。
    boardOn,
    boardSource,
    boardCards,
    boardPluginMissing,
    boardError,
    watchmanEnabled,
    watchmanWarning,
    selectedBoard,
    doneExpanded,
    boards,
    boardSummaries,
    // actions / setters：App 的 JSX、`onCreateBoard`/`onRemoveBoard`/`onOpenBoard`/
    // `onToggleBoardSwitch`/`onToggleWatchman` 与生命周期 effect 都要用。
    setBoardOn,
    setBoardSource,
    setBoardCards,
    setBoardError,
    setSelectedBoard,
    setDoneExpanded,
    setWatchmanEnabled,
    setWatchmanWarning,
    boardRpc,
    refreshBoards,
    // refs：App 的生命周期 effect 直接读——2 秒计时器取「最近一次的刷新函数」，
    // 以及「用户是否拨过值守开关」的判据。
    boardTick,
    watchmanTouchedRef,
  };
}
