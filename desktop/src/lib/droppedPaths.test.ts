import { describe, it, expect } from "vitest";
import { insertDroppedPaths, quotePathIfNeeded } from "./droppedPaths";

describe("quotePathIfNeeded", () => {
  it("路径不含空白时原样返回", () => {
    expect(quotePathIfNeeded("/Users/me/src")).toBe("/Users/me/src");
  });

  it("路径含空格时用双引号包住", () => {
    expect(quotePathIfNeeded("/Users/me/My Docs")).toBe('"/Users/me/My Docs"');
  });

  it("路径含制表符也算需要引号", () => {
    expect(quotePathIfNeeded("/Users/me/a\tb")).toBe('"/Users/me/a\tb"');
  });
});

describe("insertDroppedPaths", () => {
  it("空 draft 光标在 0 时直接插入，不加前导空格", () => {
    expect(insertDroppedPaths("", 0, ["/Users/me/src"])).toEqual({
      text: "/Users/me/src",
      caret: 13,
    });
  });

  it("前面已有内容时补一个空格", () => {
    expect(insertDroppedPaths("看看", 2, ["/Users/me/src"])).toEqual({
      text: "看看 /Users/me/src",
      caret: 16,
    });
  });

  it("前面已是空白时不重复补空格", () => {
    expect(insertDroppedPaths("看看 ", 3, ["/Users/me/src"])).toEqual({
      text: "看看 /Users/me/src",
      caret: 16,
    });
  });

  it("在中间插入时保留后缀文本，光标落在路径末尾", () => {
    expect(insertDroppedPaths("ab", 1, ["/p"])).toEqual({ text: "a /pb", caret: 4 });
  });

  it("多条各占一行", () => {
    expect(insertDroppedPaths("", 0, ["/a", "/b"])).toEqual({ text: "/a\n/b", caret: 5 });
  });

  it("含空格的那条被引号包住，其余不加", () => {
    expect(insertDroppedPaths("", 0, ["/a", "/x y"])).toEqual({
      text: '/a\n"/x y"',
      caret: 9,
    });
  });

  it("空路径数组时原样返回，光标被夹回合法范围", () => {
    expect(insertDroppedPaths("hi", 99, [])).toEqual({ text: "hi", caret: 2 });
  });

  it("caret 为负或非有限数时退回安全位置", () => {
    expect(insertDroppedPaths("hi", -5, ["/a"])).toEqual({ text: "/ahi", caret: 2 });
    expect(insertDroppedPaths("hi", Number.NaN, ["/a"])).toEqual({ text: "hi /a", caret: 5 });
  });
});
