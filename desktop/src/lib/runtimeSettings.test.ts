import { describe, expect, it } from "vitest";
import { buildPatch, parseRuntimeSettings, parseRuntimeSettingsWrite } from "./runtimeSettings";

describe("parseRuntimeSettings", () => {
  it("keeps null as null so the UI can distinguish unset from set", () => {
    const view = parseRuntimeSettings({
      path: "/home/me/.yi-agent/settings.json",
      fields: [
        {
          key: "compact.ratio",
          group: "compact",
          label: "压缩触发占比（%）",
          type: "number",
          value: null,
          default: "80",
          description: "d",
          options: [],
          min: 1,
          max: 100,
        },
      ],
    });
    expect(view.fields[0].value).toBeNull();
    expect(view.fields[0].default).toBe("80");
  });

  it("keeps a numeric zero instead of treating it as falsy", () => {
    const view = parseRuntimeSettings({
      fields: [{ key: "agent.max_turns", type: "number", value: 0 }],
    });
    expect(view.fields[0].value).toBe(0);
  });

  it("drops rows without a key and fills safe defaults for the rest", () => {
    const view = parseRuntimeSettings({
      fields: [{ type: "number", value: 1 }, { key: "agent.max_turns", value: 5 }],
    });
    expect(view.fields).toHaveLength(1);
    expect(view.fields[0].default).toBe("");
    expect(view.fields[0].options).toEqual([]);
  });

  it("tolerates a completely malformed payload", () => {
    expect(parseRuntimeSettings(null).fields).toEqual([]);
    expect(parseRuntimeSettings("nope").fields).toEqual([]);
    expect(parseRuntimeSettings({}).path).toBe("");
  });
});

describe("buildPatch", () => {
  const original = [
    {
      key: "agent.max_turns",
      group: "agent",
      label: "",
      type: "number" as const,
      value: 500,
      default: "200",
      description: "",
      options: [],
      min: 1,
      max: null,
    },
    {
      key: "compact.ratio",
      group: "compact",
      label: "",
      type: "number" as const,
      value: null,
      default: "80",
      description: "",
      options: [],
      min: 1,
      max: 100,
    },
  ];

  it("only emits fields the user actually changed", () => {
    const patch = buildPatch(original, { "agent.max_turns": "500", "compact.ratio": "" });
    expect(patch).toEqual({});
  });

  it("sends the new value for a changed field", () => {
    const patch = buildPatch(original, { "agent.max_turns": "600" });
    expect(patch).toEqual({ "agent.max_turns": "600" });
  });

  it("sends null to restore the default", () => {
    const patch = buildPatch(original, { "agent.max_turns": "" });
    expect(patch).toEqual({ "agent.max_turns": null });
  });
});

describe("parseRuntimeSettings live metadata", () => {
  it("reports an old server as unsupported instead of inventing support", () => {
    // An old host returns only fields/path. Anything other than an explicit
    // `live_apply_version: 1` must read as "not supported": guessing support
    // would let the UI promise a hot reload the host will not deliver.
    const view = parseRuntimeSettings({
      path: "/x/settings.json",
      fields: [{ key: "agent.max_turns", type: "number", value: 200 }],
    });
    expect(view.live_apply_version).toBeUndefined();
    expect(view.supported).toBe(false);
    expect(view.snapshot_origin).toBeUndefined();
    expect(view.health).toBeUndefined();
  });

  it("does not treat an unknown version number as version 1", () => {
    const view = parseRuntimeSettings({ live_apply_version: 2, fields: [] });
    expect(view.live_apply_version).toBeUndefined();
    expect(view.supported).toBe(false);
  });

  it("parses the live metadata when the host speaks version 1", () => {
    const view = parseRuntimeSettings({
      live_apply_version: 1,
      revision: "abc123",
      snapshot_origin: "last_known_good",
      health: { state: "degraded", code: "invalid_setting", invalid_keys: ["agent.max_turns"] },
      path: "/x/settings.json",
      fields: [
        {
          key: "agent.max_turns",
          type: "number",
          value: 200,
          apply_policy: "next_safe_point",
          host_value: 200,
          host_source: "override",
        },
        { key: "compact.ratio", type: "number", value: null, apply_policy: "nonsense" },
      ],
    });
    expect(view.live_apply_version).toBe(1);
    expect(view.supported).toBe(true);
    expect(view.revision).toBe("abc123");
    expect(view.snapshot_origin).toBe("last_known_good");
    expect(view.health).toEqual({
      state: "degraded",
      code: "invalid_setting",
      invalid_keys: ["agent.max_turns"],
    });
    expect(view.fields[0].apply_policy).toBe("next_safe_point");
    expect(view.fields[0].host_value).toBe(200);
    expect(view.fields[0].host_source).toBe("override");
    // An unrecognised policy must not be passed through as if it meant something.
    expect(view.fields[1].apply_policy).toBeUndefined();
  });

  it("degrades unknown metadata safely without dropping the fields", () => {
    // A malformed health object must not take the user's fields with it: the page
    // would then show "no configurable settings" and could look like data loss.
    const view = parseRuntimeSettings({
      live_apply_version: 1,
      revision: 42,
      snapshot_origin: "invented",
      health: "broken",
      fields: [{ key: "agent.max_turns", type: "number", value: 200 }],
    });
    expect(view.fields).toHaveLength(1);
    expect(view.revision).toBeUndefined();
    expect(view.snapshot_origin).toBeUndefined();
    expect(view.health).toBeUndefined();
    expect(view.supported).toBe(true);
  });
});

describe("parseRuntimeSettingsWrite", () => {
  it("marks an old server's plain ack as not live-applied", () => {
    expect(parseRuntimeSettingsWrite({ ok: true })).toEqual({
      ok: true,
      live_apply_version: undefined,
      revision: undefined,
      changed_keys: [],
      apply_policy: {},
      overridden_keys: [],
    });
  });

  it("parses the full receipt", () => {
    const result = parseRuntimeSettingsWrite({
      ok: true,
      live_apply_version: 1,
      revision: "r1",
      changed_keys: ["agent.max_turns", 7],
      apply_policy: { "agent.max_turns": "next_safe_point", other: "bogus" },
      overridden_keys: [
        { key: "compact.ratio", source: "override" },
        { key: "x", source: "made-up" },
        "garbage",
      ],
    });
    expect(result.ok).toBe(true);
    expect(result.live_apply_version).toBe(1);
    expect(result.revision).toBe("r1");
    expect(result.changed_keys).toEqual(["agent.max_turns"]);
    expect(result.apply_policy).toEqual({ "agent.max_turns": "next_safe_point" });
    expect(result.overridden_keys).toEqual([{ key: "compact.ratio", source: "override" }]);
  });

  it("treats a non-ok receipt as a failure even when it carries metadata", () => {
    expect(parseRuntimeSettingsWrite({ ok: false, live_apply_version: 1 }).ok).toBe(false);
    expect(parseRuntimeSettingsWrite(null).ok).toBe(false);
  });
});
