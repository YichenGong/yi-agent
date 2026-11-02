/**
 * resume 的「停滞检测」：连续这么久收不到该 thread 的任何帧，就判定这次
 * 全量回放已经死了。
 *
 * 为什么不是「总时长超时」：大会话的正常回放可以超过十几秒（服务端按
 * 200 条 / 256 KiB 分块下发），一刀切会在长会话上频繁误判——那正是「有时
 * 卡、有时不卡」的来源。用「连续无活动」判据，健康的长回放每收一帧就续命
 * 一次，永远不会误伤。
 *
 * 为什么不低于 10s：高丢包的移动网络下，单块重传就可能占掉几秒。
 */
export const SYNC_IDLE_TIMEOUT_MS = 15_000;

export interface Watchdog {
  /** 收到该 thread 的活动（任意帧 / 响应），重置计时。 */
  touch(): void;
  /** 停止计时。resume 落定（成功或失败）后必须调用，否则会延迟误触发。 */
  stop(): void;
  /** 是否已经判定超时。 */
  readonly fired: boolean;
}

/**
 * 只声明本模块用到的两个调用签名。
 *
 * 不直接写 `typeof setTimeout`：本仓库的 tsconfig 没有限制 `types`，`@types/node`
 * 的全局 `setTimeout` 带一个 `__promisify__` 属性，于是「一个返回句柄的普通函数」
 * 不满足 `typeof setTimeout`，注入实现就装不进去。这里只需要「排期 + 取消」两项能力。
 */
interface Timer {
  setTimeout: (fn: () => void, ms: number) => ReturnType<typeof setTimeout>;
  clearTimeout: (handle: ReturnType<typeof setTimeout>) => void;
}

const defaultTimer: Timer = {
  setTimeout: (fn, ms) => setTimeout(fn, ms),
  clearTimeout: (h) => clearTimeout(h),
};

/**
 * 建一个只会计时、不会自己重试的看门狗。`onTimeout` 保证最多调用一次。
 *
 * `timer` 可注入是为了单测能用 fake timers 之外的实现；生产一律用默认值。
 */
export function createWatchdog(
  timeoutMs: number,
  onTimeout: () => void,
  timer: Timer = defaultTimer,
): Watchdog {
  let handle: ReturnType<typeof setTimeout> | null = null;
  let fired = false;
  let stopped = false;

  const arm = (): void => {
    if (handle !== null) timer.clearTimeout(handle);
    handle = timer.setTimeout(() => {
      handle = null;
      // `stop()` 之后不该再有回调；`fired` 保证不重复上报。
      if (stopped || fired) return;
      fired = true;
      onTimeout();
    }, timeoutMs);
  };

  arm();

  return {
    touch(): void {
      // 已判死或已停机时续命没有意义：前者会掩盖已上报的失败，后者会泄漏定时器。
      if (stopped || fired) return;
      arm();
    },
    stop(): void {
      stopped = true;
      if (handle !== null) {
        timer.clearTimeout(handle);
        handle = null;
      }
    },
    get fired(): boolean {
      return fired;
    },
  };
}
