/**
 * 拖入路径的排版规则（纯函数）。
 *
 * 与 Tauri、React、DOM 都无关：这里只有字符串进出，故拖放的三种规则
 * （光标处插入、多条各占一行、含空白加引号）可以穷举单测，不需要真实拖拽。
 */

/**
 * 路径含空白字符时用双引号包住。
 *
 * 目录名带空格在 macOS 上很常见（`~/My Documents`），不包引号时人眼与下游解析
 * 都难判断路径在哪里结束。不含空白的路径保持裸形式——与用户手输 `/Users/me/src`
 * 的习惯一致，不额外加噪音。
 */
export function quotePathIfNeeded(path: string): string {
  return /\s/.test(path) ? `"${path}"` : path;
}

/** 把光标位置夹回 `[0, draft.length]`；坏值（负 / NaN / 非有限）退回安全位置。 */
function clampCaret(draft: string, caret: number): number {
  if (!Number.isFinite(caret)) return draft.length;
  return Math.max(0, Math.min(draft.length, Math.trunc(caret)));
}

/**
 * 把拖入的路径插进草稿，返回新文本与插入后的光标位置。
 *
 * 规则：光标处插入；前缀有内容且不以空白结尾时补一个空格（避免与已有文字粘连）；
 * 多条路径各占一行；含空白的路径加引号。光标落在插入块的末尾，用户拖完可以继续
 * 往下打字，不必再点一下鼠标。
 */
export function insertDroppedPaths(
  draft: string,
  caret: number,
  paths: readonly string[],
): { text: string; caret: number } {
  const at = clampCaret(draft, caret);
  if (paths.length === 0) return { text: draft, caret: at };

  const inserted = paths.map(quotePathIfNeeded).join("\n");
  const needsSpace = at > 0 && !/\s$/.test(draft.slice(0, at));
  const block = needsSpace ? ` ${inserted}` : inserted;
  return { text: draft.slice(0, at) + block + draft.slice(at), caret: at + block.length };
}
