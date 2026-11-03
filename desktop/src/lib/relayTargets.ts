/**
 * 手机端为「每个远端项目各建一条连接」的决策。
 *
 * 为什么需要这一步（spec §4.6）：桌面机上有 N 个远端项目，各自是一个独立的
 * app-server，各自在一条 relay session 上。手机只连主 session 只能看到第一个
 * 项目；要让「手机上看到的跟电脑上一样」，就得对每个可用项目各开一条连接。
 *
 * 三条规则都写在这里而不是内联在 `App.tsx` 里，因为它们各有一次踩坑史：
 * 1. `available` 为假的**不建**——那台远端还没握手，连了只会得到一个必然
 *    失败的 socket 和一条对用户毫无意义的错误。
 * 2. `session` 为 null 的**不建**——那是 `Observe` 域设备，凭据根本没发给它。
 *    建了会拿到一个空 URL。
 * 3. 连不上某个项目**不得影响其它项目**：多连接的全部价值在于一个项目坏掉
 *    时其余仍可用。
 */

/** `remote/relayTargets` 的一行。 */
export interface RelayTarget {
  profileId: string;
  label: string;
  session: string | null;
  available: boolean;
}

/**
 * 从清单里挑出「该为之建连接」的项目。
 *
 * 返回顺序与输入一致：手机上的项目顺序应当稳定，否则每次轮询都可能重排。
 */
export function connectableTargets(
  targets: readonly RelayTarget[],
): Array<RelayTarget & { session: string }> {
  return targets.flatMap((target) => {
    if (!target.available) return [];
    if (typeof target.session !== "string" || target.session.length === 0)
      return [];
    return [{ ...target, session: target.session }];
  });
}

/**
 * 校验 `remote/relayTargets` 的回包。
 *
 * 这个方法在旧版本宿主上不存在（`-32601`），在 stable 上也不存在；两种情况下
 * 手机都应表现得像「没有远端项目」，而不是报错——手机不该因为对面版本老就
 * 连本地会话都开不了。
 */
export function parseRelayTargets(raw: unknown): RelayTarget[] {
  if (typeof raw !== "object" || raw === null) return [];
  const { targets } = raw as { targets?: unknown };
  if (!Array.isArray(targets)) return [];
  return targets.flatMap((entry) => {
    if (typeof entry !== "object" || entry === null) return [];
    const { profileId, label, session, available } = entry as Record<
      string,
      unknown
    >;
    if (typeof profileId !== "string" || profileId.length === 0) return [];
    return [
      {
        profileId,
        label:
          typeof label === "string" && label.length > 0 ? label : profileId,
        session:
          typeof session === "string" && session.length > 0 ? session : null,
        available: available === true,
      },
    ];
  });
}

/**
 * 把 target 的 `session`（可能是一个完整 ws URL，也可能只是一个 id）变成可连的地址。
 *
 * 桌面侧的 `derive_relay` 产出的是**完整 URL**（`wss://relay/?session=x-y`），
 * 而早期设计文档里 `session` 只是 id。两种都接受：含 `://` 的当整条 URL 用，
 * 否则挂到 `relayUrl` 的 `session` 参数上。
 */
export function relayUrlForTarget(relayUrl: string, session: string): string {
  if (session.includes("://")) return session;
  const parsed = new URL(relayUrl);
  parsed.searchParams.set("session", session);
  return parsed.toString();
}
