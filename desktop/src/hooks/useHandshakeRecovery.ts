import type { RpcClient } from "../lib/rpc";
import { LOCAL_SLOT, type Slot } from "../lib/slots";
import type { ThreadStore } from "../lib/threadStore";
import type { ClientRegistry } from "../lib/clientRegistry";
import { isRemoteClient } from "../lib/platform";
import type { ThreadSummary, WorkspaceGroup } from "../lib/protocol";
import { formatError } from "../lib/errorMessage";
import { parseSyncPlan } from "../lib/syncPlan";
import { parseTheme } from "../lib/theme";
import { parseFontScale } from "../lib/fontScale";
import { watchmanFromSettings } from "../lib/watchman";
import { resolveTagNames } from "../lib/threadTags";
import { onboardingStatus } from "../lib/onboarding";
import { readSelectedThread } from "../lib/selectedThread";
import { writeListCache } from "../lib/sessionCache";
import { clearConnectionErrorFrom } from "../lib/connectionError";
import type { useSettingsState } from "./useSettingsState";
import type { useSessionState } from "./useSessionState";
import {
  parseSideHandshake,
  type SideConversationStore,
} from "../lib/sideConversation";
import { storedRemoteConfig } from "../lib/remoteConfig";

type SettingsState = ReturnType<typeof useSettingsState>;
type SessionState = ReturnType<typeof useSessionState>;

/**
 * 一次完整握手的**全部**逻辑（原 `App.tsx` 巨型 effect 内 `handshake`，
 * 221 行，L1948–L2170）。每次重连都要重放它，因为新进程的记忆是空的。
 *
 * 与 `useNotificationRouter` 同样做成「工厂」而非 hook，理由见那里的说明：
 * 原闭包捕获的是 effect 运行那一帧的 `sess`/`prefs`，改成 hook 会
 * 改变取值时机；做成工厂则由调用方在 effect 里现建，依赖由参数显式传入。
 */
export interface HandshakeRecoveryDeps {
  activeSlotRef: React.MutableRefObject<Slot>;
  clientRef: React.MutableRefObject<RpcClient | null>;
  registryRef: React.MutableRefObject<ClientRegistry<RpcClient>>;
  syncPlanRef: React.MutableRefObject<unknown>;
  leadEnsured: React.MutableRefObject<boolean>;
  sess: SessionState;
  prefs: SettingsState;
  store: ThreadStore;
  isMobile: boolean;
  setStatus: (v: string) => void;
  setTagNames: (v: Record<"red" | "orange" | "yellow" | "green" | "blue" | "purple", string>) => void;
  setCurrentError: (msg: string) => void;
  setBuildKind: (v: "dev" | "nogit" | "release") => void;
  setLeadAgentEnabled: (v: boolean) => void;
  /** 用户是否已拨过后台值守开关；拨过就不再被迟到的 read 覆盖。 */
  watchmanTouchedRef: React.MutableRefObject<boolean>;
  setWatchmanEnabled: (v: boolean) => void;
  force: (updater: (v: number) => number) => void;
  selectThread: (id: string, opts?: { forceResume?: boolean }) => Promise<void>;
  connectRemoteTargets: (client: RpcClient) => Promise<void>;
  scheduleReconnect: () => void;
  /** 退避计数清零（握手成功即复位）。 */
  resetAttempt: () => void;
  buildResumeHints: () => unknown;
  /** 旁支绑定登记处。每条连接各自解析能力与宿主 id，不借 globalReader。 */
  sideStore: SideConversationStore;
}

export function createHandshakeRecovery(deps: HandshakeRecoveryDeps) {
  const {
    activeSlotRef,
    clientRef,
    registryRef,
    syncPlanRef,
    leadEnsured,
    sess,
    prefs,
    store,
    isMobile,
    setStatus,
    setTagNames,
    setCurrentError,
    setBuildKind,
    setLeadAgentEnabled,
    watchmanTouchedRef,
    setWatchmanEnabled,
    force,
    selectThread,
    connectRemoteTargets,
    scheduleReconnect,
    resetAttempt,
    buildResumeHints,
    sideStore,
  } = deps;

  return async (client: RpcClient) => {
      try {
        const initialized = await client.request<Record<string, unknown>>(
          "initialize",
          {
            ...(isRemoteClient() ? { resumeHints: buildResumeHints() } : {}),
          },
        );
        syncPlanRef.current = parseSyncPlan(initialized);
        // 旁支能力与宿主身份必须按**这条连接**解析（spec §8.1）：本地和每个远端
        // 项目各跑一个 app-server，它们各自的能力位与随机宿主 id 互不相干。借
        // `globalReader`（永远本地）的答案，会让一台还没升级的远端项目看起来
        // 「支持旁支」，点下去每一条请求都是 method not found。
        //
        // scope 取持久化的配对结果：远端设备的能力由 token 决定，本地 stdio
        // 连接始终是 Admin。缺省按 control（与配对协议一致）。
        const slot = activeSlotRef.current;
        const scope = isRemoteClient()
          ? (storedRemoteConfig(localStorage)?.scope ?? "control")
          : "admin";
        const sideBinding = parseSideHandshake(slot, client, initialized, scope);
        if (sideBinding) sideStore.bind(sideBinding);
        // 构建类型只在 dev 上开放「实验功能」；读失败按 release 处理（fail-closed，
        // 与 Rust 侧的硬闸同一口径），不让面板因为一次网络抖动冒出来。
        // 全局只读一律问**本地**槽位（spec §5.2）：远端跑的是它自己的二进制，
        // 它报的 build kind 与 flag 值描述的是那台机器。拿远端的答案渲染本机的
        // 「实验功能」面板，会得到一个恰好看起来合理、来源却完全错误的值。
        const globalReader = registryRef.current.forGlobalReads() ?? client;
        try {
          const info = await globalReader.request<{ build_kind?: unknown }>(
            "build/info",
            {},
          );
          const kind = info.build_kind;
          setBuildKind(kind === "dev" || kind === "nogit" ? kind : "release");
        } catch {
          setBuildKind("release");
        }
        // 组长开关：与 `build/info` 同一时机读一次。失败按关处理（fail-closed），
        // 与 Rust 侧 `is_lead_agent_enabled(None)` 同一口径。
        // 局部量而不是直接读 state：`setLeadAgentEnabled` 要下一次渲染才生效，
        // 而本函数后面的补建循环就在**这一轮**里跑，读 state 拿到的还是 `null`。
        let leadOn = false;
        try {
          const flags = await globalReader.request<{
            flags?: Array<{ name?: unknown; enabled?: unknown }>;
          }>("flags/list", {});
          const lead = (flags.flags ?? []).find((f) => f.name === "lead_agent");
          leadOn = lead?.enabled === true;
        } catch {
          leadOn = false;
        }
        setLeadAgentEnabled(leadOn);
        const settings = await client.request<{
          theme?: unknown;
          resident_daemon_watchman_enabled?: unknown;
          /** 通用化之前的旧字段名：旧宿主仍只回它。 */
          board_watchman_enabled?: unknown;
          relay_url?: unknown;
          tag_names?: unknown;
          /** 字号档位：两个平台各一个键，客户端只认自己那个。 */
          font_scale_desktop?: unknown;
          font_scale_ios?: unknown;
        }>("ui/settings/read", {});
        // 中继地址没有「用户正在改」的触碰标记：设置页是模态的，首屏 read 回来
        // 时用户还没法编辑它，直接采纳权威值即可。未配置（null）→ 空串。
        prefs.setRelayUrl(
          typeof settings.relay_url === "string" ? settings.relay_url : "",
        );
        // 只有在此之后没有更新的主题选择时才采纳权威值：read 在途期间用户改了
        // 主题或收到 ui/settings/updated，更新的那个才是当前选择。
        if (!prefs.themeTouchedRef.current) {
          const authoritative = parseTheme(settings.theme);
          prefs.confirmedThemeRef.current = authoritative;
          prefs.setTheme(authoritative);
        }
        // 字号：只认本平台那个键。`parseFontScale` 对未设置/非法值返回 `null`，
        // 此时**保持当前值**（首屏缓存或平台默认），不要把它抹成默认——
        // 那会让手机端刚改的档位在握手后被悄悄重置。
        prefs.setFontScaleFromServer(
          parseFontScale(
            isMobile ? settings.font_scale_ios : settings.font_scale_desktop,
          ),
        );
        // 值守：直接读这份 settings（同一趟请求里就有），不再多发一次
        // `ui/settings/read`。读序（新名优先、旧名兜底）与缺省口径都在
        // `watchmanFromSettings` 里，与 `readWatchman` 共用同一份实现。
        // 同样只在用户还没拨过开关时采纳：重启重放握手时面板已在屏上，read 在途
        // 期间拨下的开关不能被这趟迟到的旧值悄悄覆盖回原样。
        if (!watchmanTouchedRef.current) {
          setWatchmanEnabled(
            watchmanFromSettings(settings as Record<string, unknown>),
          );
        }
        // 标签显示名：服务端只回用户覆盖项，与默认名合并成完整 6 键。
        setTagNames(
          resolveTagNames(
            settings.tag_names && typeof settings.tag_names === "object"
              ? (settings.tag_names as Record<string, string>)
              : null,
          ),
        );
        // 首启引导：读不到状态(宿主老、RPC 报错、字段缺失)一律按「不需要」处理，
        // 绝不因为一个读失败弹向导、更不挡住主界面。
        try {
          const status = await onboardingStatus((m, p) =>
            (clientRef.current as RpcClient).request(m, p),
          );
          prefs.setOnboarding({
            needed: status.needed && !status.dismissed,
            reasons: status.reasons,
          });
        } catch {
          prefs.setOnboarding({ needed: false, reasons: [] });
        }
        await sess.refreshWorkspaces();
        const list = await client.request<{
          groups: WorkspaceGroup[];
          pinned?: ThreadSummary[];
        }>("thread/listAll", {});
        sess.setGroups(list.groups);
        sess.setPinned(list.pinned ?? []);
        store.seed(list.groups.flatMap((g) => g.threads));
        if (isRemoteClient())
          writeListCache(
            localStorage,
            list.groups,
            list.pinned ?? [],
            store.currentId,
          );
        // 手机端：为每个可用远端项目各建一条连接（spec §4.6）。
        //
        // 桌面机上有 N 个远端项目，各自一个 app-server、各占一条 relay session。
        // 只连主 session 的话手机上只能看到第一个项目，与「手机上看到的跟电脑上
        // 一样」不符。**不 await**：多建 N 条 ws 不该推迟首屏；失败只影响它自己。
        if (isRemoteClient()) {
          void connectRemoteTargets(client);
        }
        // 已有会话的项目目录不会被服务端的 `listAll` 懒创建覆盖（那会改写它既
        // 有的分组语义），故这类项目的组长入口要靠客户端补：对「一个组长都没
        // 有」且确实存在的目录组逐个 `thread/lead`，再重取一次列表把新组长带出
        // 来。**不 await 在首屏关键路径上**——补组长是锦上添花，绝不能推迟首屏
        // 渲染或扣住后续的 selectThread；失败一律忽略。只在整个进程里补一次
        // （`leadEnsured`），服务端创建本身幂等。
        // 组长只在本地槽位补：`thread/lead` 对远端项目**不做**（spec §7），
        // 且 `cwd` 是本机路径，送到远端必然失败。
        if (
          !leadEnsured.current &&
          leadOn &&
          activeSlotRef.current === LOCAL_SLOT
        ) {
          const needsLead = list.groups.filter(
            (g) => g.exists && !g.threads.some((t) => t.role === "lead"),
          );
          if (needsLead.length > 0) {
            leadEnsured.current = true;
            void (async () => {
              for (const g of needsLead) {
                try {
                  await client.request("thread/lead", { cwd: g.workspace });
                } catch {
                  // 单个目录补建失败只影响它自己的组长条目，继续下一个。
                }
              }
              try {
                const patched = await client.request<{
                  groups: WorkspaceGroup[];
                  pinned?: ThreadSummary[];
                }>("thread/listAll", {});
                sess.setGroups(patched.groups);
                store.seed(patched.groups.flatMap((g) => g.threads));
              } catch {
                // 重取失败就保留首屏那一份：至少侧栏照常可用。
              }
            })();
          }
        }
        // 置顶分区在最上方，服务端给的顺序就是首屏该选中的第一个。
        // 握手（含重连/切回前台）**不是**「用户换了页面」：当前会话——冷启从缓存
        // 恢复的、或用户正看着的——必须保持，把它踢回第一条会让人丢掉正在看的
        // 东西，桌面端与手机端一致。并且重连后新进程记忆为空（warm 与订阅窗口在
        // exited 时已清），必须对它再走一次 selectThread：冷则补 resume、running
        // 则补 readItems（不打断在途回合）、warm 则补 subscribe，否则视图留着而
        // 该会话的实时更新会静默停摆。仅当当前会话不在服务端新列表里（被删）或
        // 根本没有已选会话时，才回落第一条。
        // 回落顺序：当前会话 → 上次选中的（冷启动没有当前会话）→ 列表第一条。
        // 记住值同样要过服务端列表这一关：会话可能已被别的端删掉。
        const keptId = sess.currentIdRef.current ?? readSelectedThread();
        const kept =
          keptId !== null &&
          list.groups.some((g) =>
            g.threads.some((t) => t.thread_id === keptId),
          );
        if (kept) {
          await selectThread(keptId!);
        } else {
          const first =
            (list.pinned ?? [])[0] ?? list.groups.flatMap((g) => g.threads)[0];
          if (first) await selectThread(first.thread_id);
        }
        // 否则保持空态,等用户选目录新建(设计 §7.2:不再自动在 $HOME 建对话)。
        // 握手走通才算真的连上:清零退避,下一次断开从 500ms 重新起算。
        resetAttempt();
        setStatus("connected");
        // 上一条「连接已断开」现在过期了：自动重连常常几秒就成功，留着那条红条
        // 会让人以为问题还在、还在等。只收连接类文案——服务端拒绝、参数错误那类
        // 与连接无关，仍要让人看见。
        let cleared = false;
        for (const id of store.ids()) {
          if (clearConnectionErrorFrom(store.view(id).session)) cleared = true;
        }
        if (cleared) force((v) => v + 1);
        // 真的连上了：重新武装"凭据被移除"的自动提示。此前用户可能已点过返回
        // （那次拒绝的对象是旧凭据）；此后凭据若再失效，是一件新事，该再提示一次。
        prefs.repairDismissedRef.current = false;
      } catch (e) {
        const msg = formatError(e);
        setCurrentError(msg);
        setStatus(`error: ${msg}`);
        // 握手失败同样是"没连上"：必须排队重试，否则会永久停在 error。
        // （经中继时"电脑未连接"正是这条路径——见 rpc.ts 的无 id 错误帧。）
        scheduleReconnect();
      }
  };
}
