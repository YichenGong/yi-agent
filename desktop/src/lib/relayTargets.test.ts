import { describe, expect, it } from "vitest";
import {
  connectableTargets,
  parseRelayTargets,
  relayUrlForTarget,
  type RelayTarget,
} from "./relayTargets";

const target = (over: Partial<RelayTarget> = {}): RelayTarget => ({
  profileId: "proj-foo",
  label: "foo",
  session: "wss://relay/?session=base-proj-foo",
  available: true,
  ...over,
});

describe("connectableTargets", () => {
  it("keeps available targets that carry a session", () => {
    expect(connectableTargets([target()])).toHaveLength(1);
  });

  /**
   * `available` 为假说明那台远端的进程还没握手。连了只会在手机上留一条
   * 必然失败的 socket 和一句用户读不出因果的报错。
   */
  it("skips an unavailable target", () => {
    expect(connectableTargets([target({ available: false })])).toEqual([]);
  });

  /**
   * `session` 为 null = 这台设备是 `Observe` 域。凭据根本没发给它，建连接
   * 只会得到一个空地址。
   */
  it("skips a target with no session (Observe scope)", () => {
    expect(connectableTargets([target({ session: null })])).toEqual([]);
  });

  it("skips an empty session string as well as null", () => {
    expect(connectableTargets([target({ session: "" })])).toEqual([]);
  });

  it("keeps the input order", () => {
    const got = connectableTargets([
      target({ profileId: "b" }),
      target({ profileId: "a" }),
    ]);
    expect(got.map((t) => t.profileId)).toEqual(["b", "a"]);
  });

  /** 一个坏的不影响其余的：多连接的价值就在于一个项目坏掉时其余仍可用。 */
  it("one unusable target does not drop the others", () => {
    const got = connectableTargets([
      target({ profileId: "bad", available: false }),
      target({ profileId: "good" }),
    ]);
    expect(got.map((t) => t.profileId)).toEqual(["good"]);
  });
});

describe("parseRelayTargets", () => {
  it("reads a well-formed response", () => {
    expect(
      parseRelayTargets({
        targets: [
          {
            profileId: "proj-foo",
            label: "foo",
            session: "wss://relay/?session=s-foo",
            available: true,
          },
        ],
      }),
    ).toEqual([
      {
        profileId: "proj-foo",
        label: "foo",
        session: "wss://relay/?session=s-foo",
        available: true,
      },
    ]);
  });

  /**
   * 旧宿主 / stable 根本没有这个方法。手机不该因此连本地会话都开不了，
   * 所以任何形状不对的输入都退化成「没有远端项目」。
   */
  it("degrades to an empty list for junk input", () => {
    for (const raw of [
      null,
      undefined,
      "nope",
      {},
      { targets: "x" },
      { targets: [1, null] },
    ]) {
      expect(parseRelayTargets(raw)).toEqual([]);
    }
  });

  it("drops entries without a profile id", () => {
    expect(
      parseRelayTargets({ targets: [{ label: "x", available: true }] }),
    ).toEqual([]);
  });

  it("falls back to the id when the label is blank", () => {
    const [row] = parseRelayTargets({
      targets: [{ profileId: "p", label: "", available: true }],
    });
    expect(row.label).toBe("p");
  });

  /** 非布尔 `available` 一律当「不可用」：fail-closed，不能猜。 */
  it("treats a non-boolean available as false", () => {
    const [row] = parseRelayTargets({
      targets: [{ profileId: "p", available: "yes" }],
    });
    expect(row.available).toBe(false);
  });

  it("treats a non-string session as null", () => {
    const [row] = parseRelayTargets({
      targets: [{ profileId: "p", session: 42 }],
    });
    expect(row.session).toBeNull();
  });
});

describe("relayUrlForTarget", () => {
  /** 桌面侧 `derive_relay` 产出的就是完整 URL，直接可用。 */
  it("uses a full url as-is", () => {
    expect(relayUrlForTarget("wss://relay/", "wss://relay/?session=a-b")).toBe(
      "wss://relay/?session=a-b",
    );
  });

  /** 只有一个 id 时挂到本机 relay 的 session 参数上。 */
  it("attaches a bare session id to the relay url", () => {
    expect(relayUrlForTarget("wss://relay/ws", "a-b")).toBe(
      "wss://relay/ws?session=a-b",
    );
  });

  /** 已有的 `session` 必须被替换，不能追加。 */
  it("replaces an existing session parameter", () => {
    const url = relayUrlForTarget("wss://relay/ws?session=old", "new");
    expect(url.match(/session=/g)).toHaveLength(1);
    expect(url).toContain("session=new");
  });
});
