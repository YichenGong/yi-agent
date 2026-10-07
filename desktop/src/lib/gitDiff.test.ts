import { describe, it, expect } from "vitest";
import { parseUnifiedDiff } from "./gitDiff";

describe("parseUnifiedDiff", () => {
  it("splits files and assigns old/new line numbers", () => {
    const text = [
      "diff --git a/a.txt b/a.txt",
      "index 111..222 100644",
      "--- a/a.txt",
      "+++ b/a.txt",
      "@@ -1,3 +1,3 @@",
      " one",
      "-two",
      "+TWO",
      " three",
      "",
    ].join("\n");
    const files = parseUnifiedDiff(text);
    expect(files).toHaveLength(1);
    expect(files[0].path).toBe("a.txt");
    expect(files[0].hunks[0].lines).toEqual([
      { kind: "ctx", oldNo: 1, newNo: 1, text: "one" },
      { kind: "del", oldNo: 2, newNo: null, text: "two" },
      { kind: "add", oldNo: null, newNo: 2, text: "TWO" },
      { kind: "ctx", oldNo: 3, newNo: 3, text: "three" },
    ]);
  });

  it("marks new files and ignores the no-newline marker", () => {
    const text = [
      "diff --git a/n.txt b/n.txt",
      "new file mode 100644",
      "--- /dev/null",
      "+++ b/n.txt",
      "@@ -0,0 +1,1 @@",
      "+hi",
      "\\ No newline at end of file",
      "",
    ].join("\n");
    const files = parseUnifiedDiff(text);
    expect(files[0].status).toBe("A");
    expect(files[0].additions).toBe(1);
    expect(files[0].hunks[0].lines).toEqual([
      { kind: "add", oldNo: null, newNo: 1, text: "hi" },
    ]);
  });

  it("marks binary files and tolerates an empty diff", () => {
    expect(parseUnifiedDiff("")).toEqual([]);
    const text = [
      "diff --git a/img.png b/img.png",
      "Binary files a/img.png and b/img.png differ",
      "",
    ].join("\n");
    const files = parseUnifiedDiff(text);
    expect(files[0].binary).toBe(true);
    expect(files[0].hunks).toEqual([]);
  });

  it("parses multiple files in one diff", () => {
    const text = [
      "diff --git a/a b/a",
      "--- a/a",
      "+++ b/a",
      "@@ -1 +1 @@",
      "-x",
      "+y",
      "diff --git a/b b/b",
      "--- a/b",
      "+++ b/b",
      "@@ -1 +1 @@",
      "-p",
      "+q",
      "",
    ].join("\n");
    const files = parseUnifiedDiff(text);
    expect(files.map((f) => f.path)).toEqual(["a", "b"]);
  });
});
