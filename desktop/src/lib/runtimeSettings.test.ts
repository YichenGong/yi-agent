import { describe, expect, it } from "vitest";
import { buildPatch, parseRuntimeSettings } from "./runtimeSettings";

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
