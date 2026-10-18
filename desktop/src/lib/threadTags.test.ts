import { describe, expect, it } from "vitest";
import {
  DEFAULT_TAG_NAMES,
  TAG_DOT_CLASS,
  TAG_IDS,
  TAG_NAME_MAX_LEN,
  isValidTagId,
  resolveTagNames,
  tagLabel,
  type TagId,
} from "./threadTags";

describe("threadTags", () => {
  it("has exactly six ids in the fixed menu order", () => {
    expect(TAG_IDS).toEqual(["red", "orange", "yellow", "green", "blue", "purple"]);
  });

  it("defines a name and a dot class for every id", () => {
    expect(Object.keys(DEFAULT_TAG_NAMES).sort()).toEqual([...TAG_IDS].sort());
    expect(Object.keys(TAG_DOT_CLASS).sort()).toEqual([...TAG_IDS].sort());
    for (const id of TAG_IDS) {
      expect(DEFAULT_TAG_NAMES[id].length).toBeGreaterThan(0);
      expect(DEFAULT_TAG_NAMES[id].length).toBeLessThanOrEqual(TAG_NAME_MAX_LEN);
      expect(TAG_DOT_CLASS[id]).toMatch(/^bg-/);
    }
  });

  it("accepts the six ids and rejects everything else", () => {
    for (const id of TAG_IDS) expect(isValidTagId(id)).toBe(true);
    for (const bad of ["gray", "RED", "", null, undefined, 1, {}, [], true]) {
      expect(isValidTagId(bad)).toBe(false);
    }
  });

  it("resolves missing overrides back to the defaults", () => {
    expect(resolveTagNames({})).toEqual(DEFAULT_TAG_NAMES);
    expect(resolveTagNames(null)).toEqual(DEFAULT_TAG_NAMES);
    expect(resolveTagNames(undefined)).toEqual(DEFAULT_TAG_NAMES);

    const partial = resolveTagNames({ red: "紧急" });
    expect(partial.red).toBe("紧急");
    // 部分覆盖不得丢掉其它 5 个键。
    expect(partial.orange).toBe(DEFAULT_TAG_NAMES.orange);
    expect(Object.keys(partial).sort()).toEqual([...TAG_IDS].sort());
  });

  it("ignores blank and unknown-key overrides", () => {
    const names = resolveTagNames({ red: "   ", gray: "灰" } as Record<string, string>);
    expect(names.red).toBe(DEFAULT_TAG_NAMES.red);
    expect("gray" in names).toBe(false);
  });

  it("trims overrides", () => {
    expect(resolveTagNames({ blue: "  参考  " }).blue).toBe("参考");
  });

  it("labels a tag, falling back to the default name", () => {
    expect(tagLabel("red", resolveTagNames({ red: "紧急" }))).toBe("紧急");
    expect(tagLabel("red", resolveTagNames({}))).toBe(DEFAULT_TAG_NAMES.red);
    // 名字表缺键也不抛。
    expect(tagLabel("blue", {} as unknown as Record<TagId, string>)).toBe(
      DEFAULT_TAG_NAMES.blue,
    );
  });
});