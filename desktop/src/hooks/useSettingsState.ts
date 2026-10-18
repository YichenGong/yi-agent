import { useEffect, useRef, useState } from "react";
import { isRemoteClient } from "../lib/platform";
import { applyTheme, readCachedTheme, type Theme } from "../lib/theme";
import { formatError } from "../lib/errorMessage";
import type { RpcClient } from "../lib/rpc";

/**
 * 设置关注点：设置页与配对界面的开合、首启引导、本机侧车的中继地址、主题。
 *
 * 归属判据是「这是**哪台**客户端的设置」——答案只有一个：本机。所以这批状态
 * 没有会话维度，也不随切会话重置：`settingsOpen` 是模态开合；`relayUrl`/`theme`
 * 是本机偏好（权威在服务端 `preferences.json`，localStorage 只是首屏缓存）；
 * `onboarding` 是首启一次性的引导；
 * `pairingVersion`/`repairing`/`repairNotice` 是「这台设备还能不能用」的配对态。
 *
 * `rpc` 是 4a-4d 那条「取客户端」的 getter：未连接时返回 null。`changeTheme`
 * 逐字保留搬移前的形状——它**不**`return`，而是直接 `.then(ok, fail)`：主题是
 * 乐观更新，写入失败必须自己回退到已确认的基线，静默放弃会把界面留在服务端并
 * 不认可的主题上（故与 diff/进程那套「读不到就静默」的语义不同）。
 *
 * `force`/`setCurrentError` 不是本关注点的状态，而是 `changeTheme` 写失败时落回
 * App 层的那两笔写入（对话里的红字、强制重渲染 tick），故由调用方注入，与
 * `useSessionState`/`useProcessState` 同一口径。`saveRelayUrl` 的两条守卫
 * （`isRemoteClient()` 与动态 `invoke`）逐字保留：只有桌面构建有侧车与那条 Tauri
 * 命令，iOS 是纯 ws 客户端，接缝传 undefined 让保存按钮不可用。
 *
 * 跨关注点的编排留在 App（生命周期 effect 与 JSX）：`ui/settings/updated` 的跟随、
 * `ui/settings/read` 的采纳（含「用户已触碰过，旧值不得覆盖」的判据）、4401 进重配、
 * 握手成功时重新武装提示、`OnboardingWizard` 的 `onDone`（它还要 `restartSidecar`/
 * `modelCall`）都还在那边。故本 hook 把 `setTheme`/`themeTouchedRef`/
 * `confirmedThemeRef`/`setOnboarding`/`setRepairing`/`setRepairNotice`/`setRelayUrl`/
 * `repairingRef`/`repairDismissedRef` 一并返回，供那些现场直接读改写。
 */
export function useSettingsState({
  rpc,
  force,
  setCurrentError,
}: {
  /** 取当前的 RPC 客户端；未连接即 null（`changeTheme` 据此把写入整段跳过）。 */
  rpc: () => RpcClient | null;
  /** App 的强制重渲染 tick（主题写失败回退后要它把界面刷新）。 */
  force: (updater: (v: number) => number) => void;
  /** 往当前会话的 session 上写一条错误（主题写失败的那条红字）。 */
  setCurrentError: (msg: string) => void;
}) {
  // 首启引导：`needed && !dismissed` 时用向导盖住主界面。读失败一律按「不需要」
  // 处理，绝不因为一个读失败挡住主界面（见 handshake）。
  const [onboarding, setOnboarding] = useState<{ needed: boolean; reasons: string[] }>({
    needed: false,
    reasons: [],
  });
  // iOS 首启：没有远端配置时先配对。版本号是「让 transport 重算」的触发器——
  // 配对落盘后加一，下面那个 effect 才第一次真正建客户端（此前被
  // `needsPairing` 挡住），这一次读到的是 ws 配置。
  const [pairingVersion, setPairingVersion] = useState(0);
  /**
   * 手机端「重新配对」：true 时全屏呈现配对界面。
   *
   * **非破坏性**——不动已存配置，提交成功才覆盖；点「返回」即回原离线态。
   * 首启配对走 `needsPairing`，与此独立（首启优先）。
   */
  const [repairing, setRepairing] = useState(false);
  /** 自动进入重配时展示的原因（如凭据被移除）；手动进入或返回后清空。 */
  const [repairNotice, setRepairNotice] = useState<string | null>(null);
  /**
   * effect 闭包读不到最新 state，故用 ref 判断「是否已在重配界面」，避免重复进入。
   * 每次渲染同步一次。
   */
  const repairingRef = useRef(false);
  repairingRef.current = repairing;
  /**
   * 用户是否已对"此设备已被移除"这一自动提示做过回答（点了返回）。
   *
   * 没有它，「返回」形同虚设：后台退避重连每轮都会再撞上 4401，于是每轮都把
   * 用户弹回去——用户的决定被反复撤销。握手成功（真的连上了）时重新武装，
   * 因为那之后凭据若再失效就是一件新事，该再提示一次。
   */
  const repairDismissedRef = useRef(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // 「本机侧车要连的中继地址」：首屏由 ui/settings/read 回填，用户在设置页改后
  // 经 Tauri `set_relay_url` 落盘并触发侧车重启。
  const [relayUrl, setRelayUrl] = useState("");
  // 首屏先用缓存渲染，连上后以 ui/settings/read 的权威值为准。
  const [theme, setTheme] = useState<Theme>(() => readCachedTheme() ?? "dark");
  // 挂载后主题是否已被更新的选择触碰过（ui/settings/updated 通知或用户
  // changeTheme）。首屏 read 在途时若被触碰，read 返回的旧值不得覆盖它。
  const themeTouchedRef = useRef(false);
  // 服务端已确认（或首屏 read 采纳）的主题，即「乐观更新可以回退到」的基线。
  // 用 ref 而非 state：写失败的回调读到的是最新基线，而不是某次调用时刻的
  // 陈旧闭包值——否则并发写会退回到中间态（见 changeTheme）。
  const confirmedThemeRef = useRef<Theme>(readCachedTheme() ?? "dark");

  /**
   * 保存侧车的中继地址（设置页「远程访问」的保存按钮调用）。
   *
   * 只有桌面构建有侧车与这条 Tauri 命令；iOS 是纯 ws 客户端，接缝传 undefined
   * 让保存按钮不可用（与 `pickDirectory` 的守卫同一口径）。落盘由宿主完成，并会
   * 杀掉侧车让监管循环用新值重启；成功后本地状态跟上，失败原样抛出由组件内联提示。
   */
  const saveRelayUrl = async (value: string | null): Promise<void> => {
    if (isRemoteClient()) throw new Error("远程客户端不支持配置本机侧车");
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("set_relay_url", { url: value });
    setRelayUrl(value ?? "");
  };

  const changeTheme = (next: Theme) => {
    // 立即生效，再落盘。
    themeTouchedRef.current = true;
    setTheme(next);
    rpc()?.request("ui/settings/write", { theme: next }).then(
      () => {
        // 服务端已接受：把它记作新的回退基线。
        confirmedThemeRef.current = next;
      },
      (e) => {
        // 写失败：回退到**最近一次已确认**的主题，而不是本次调用时刻的闭包值。
        // 并发写时后者会退回到中间态（例如先浅后深都失败，却停在深）。
        setTheme(confirmedThemeRef.current);
        setCurrentError(formatError(e));
        force((v) => v + 1);
      },
    );
  };

  useEffect(() => {
    applyTheme(theme);
  }, [theme]);

  return {
    // state：设置/配对 JSX 与生命周期 effect 直接消费。
    onboarding,
    pairingVersion,
    repairing,
    repairNotice,
    settingsOpen,
    relayUrl,
    theme,
    // setters：JSX 与生命周期 effect 直接写（跟随推送主题、采纳首屏权威值、
    // 进/出重配、开合设置页、回填中继地址）。
    setOnboarding,
    setPairingVersion,
    setRepairing,
    setRepairNotice,
    setSettingsOpen,
    setRelayUrl,
    setTheme,
    // refs：一次性通知闭包与渲染期读它们——主题是否已被触碰/已确认的回退基线、
    // 是否已在重配界面、用户是否已答过那次 4401 自动提示。
    repairingRef,
    repairDismissedRef,
    themeTouchedRef,
    confirmedThemeRef,
    // actions：JSX 直接消费（设置页的主题切换与中继地址保存）。
    saveRelayUrl,
    changeTheme,
  };
}
