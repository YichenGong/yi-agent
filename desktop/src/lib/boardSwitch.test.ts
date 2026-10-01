import { describe, expect, it } from "vitest";
import { formatSwitch, resolveSwitch } from "./boardSwitch";

describe("resolveSwitch", () => {
  it("lets the project layer win over the global layer", () => {
    expect(resolveSwitch(true, false)).toEqual({ value: false, source: "project" });
    expect(resolveSwitch(false, true)).toEqual({ value: true, source: "project" });
  });

  it("inherits the global layer when the project layer is unset", () => {
    expect(resolveSwitch(true, null)).toEqual({ value: true, source: "global" });
  });

  it("defaults to disabled when both layers are unset", () => {
    expect(resolveSwitch(null, null)).toEqual({ value: false, source: "default" });
  });
});

describe("formatSwitch", () => {
  it("names the feature and shows the source", () => {
    const text = formatSwitch(true, "project");
    expect(text).toContain("Superpowers 看板");
    expect(text).toContain("on");
    expect(text).toContain("project");
  });

  it("says off when disabled", () => {
    expect(formatSwitch(false, "default")).toContain("off");
  });
});
