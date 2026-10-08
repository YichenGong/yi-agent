/**
 * unified diff → 结构化文件列表。
 *
 * 纯函数：把 git 输出的文本折成可渲染的 `FileDiff[]`。渲染层只消费结构，
 * 不重新解析文本，故解析规则可脱离 DOM 单测。
 */

export interface DiffLine {
  kind: "add" | "del" | "ctx";
  oldNo: number | null;
  newNo: number | null;
  text: string;
}

export interface Hunk {
  header: string;
  lines: DiffLine[];
}

export interface FileDiff {
  path: string;
  status: string;
  additions: number;
  deletions: number;
  binary: boolean;
  hunks: Hunk[];
}

/** 从 `diff --git a/x b/y` 里取新路径；新文件取 `+++ b/<path>`。 */
function pathFromGitLine(line: string): string | null {
  const m = /^diff --git a\/(.+) b\/(.+)$/.exec(line);
  return m ? m[2] : null;
}

export function parseUnifiedDiff(text: string): FileDiff[] {
  const files: FileDiff[] = [];
  let current: FileDiff | null = null;
  let hunk: Hunk | null = null;
  let oldNo = 0;
  let newNo = 0;

  // 返回新建的文件对象、由调用处赋值给 `current`：TS 的控制流分析看不到仅
  // 在闭包内对 `current` 的赋值，会把后续使用错误地窄化成 `never`。
  const startFile = (path: string): FileDiff => {
    const next: FileDiff = { path, status: "M", additions: 0, deletions: 0, binary: false, hunks: [] };
    files.push(next);
    hunk = null;
    return next;
  };

  for (const raw of text.split("\n")) {
    if (raw.startsWith("diff --git ")) {
      const path = pathFromGitLine(raw);
      if (path) current = startFile(path);
      continue;
    }
    if (!current) continue;

    if (raw.startsWith("new file mode")) {
      current.status = "A";
      continue;
    }
    if (raw.startsWith("deleted file mode")) {
      current.status = "D";
      continue;
    }
    if (raw.startsWith("rename from ") || raw.startsWith("rename to ")) {
      current.status = "R";
      continue;
    }
    if (raw.startsWith("Binary files ") || raw.startsWith("GIT binary patch")) {
      current.binary = true;
      continue;
    }
    if (raw.startsWith("+++ ")) {
      // 用 +++ 的新路径补正（对 /dev/null 应保持 new file 语义，跳过）。
      const p = raw.slice(4).trim();
      if (p !== "/dev/null" && current.hunks.length === 0) current.path = p.replace(/^b\//, "");
      continue;
    }
    if (raw.startsWith("--- ") || raw.startsWith("index ") || raw.startsWith("similarity ")) {
      continue;
    }
    if (raw.startsWith("@@")) {
      const m = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
      oldNo = m ? Number(m[1]) : 0;
      newNo = m ? Number(m[2]) : 0;
      hunk = { header: raw, lines: [] };
      current.hunks.push(hunk);
      continue;
    }
    if (raw.startsWith("\\ No newline")) continue;

    if (hunk === null) continue;
    const marker = raw[0];
    const body = raw.length > 0 ? raw.slice(1) : "";
    if (marker === "+") {
      hunk.lines.push({ kind: "add", oldNo: null, newNo: newNo++, text: body });
      current.additions++;
    } else if (marker === "-") {
      hunk.lines.push({ kind: "del", oldNo: oldNo++, newNo: null, text: body });
      current.deletions++;
    } else if (marker === " ") {
      hunk.lines.push({ kind: "ctx", oldNo: oldNo++, newNo: newNo++, text: body });
    } else if (raw === "") {
      // 尾随空行，忽略。
    }
  }
  // 二进制文件时不保留空 hunk 的伪造头。
  for (const f of files) if (f.binary) f.hunks = [];
  return files;
}
