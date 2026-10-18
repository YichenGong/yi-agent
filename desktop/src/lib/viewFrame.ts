/** 前端渲染原语：帧的 `asKind` 只是「声明」，真正渲染用哪个由内容说了算。 */
export type ViewKind = "diff" | "log" | "text";

/**
 * 后端 `ViewFrame` 的线协议形状（驼峰）。字段名与 `yi-agent-app-server` 的
 * `view_frame_serializes_as_camel_case` 逐字对齐：少一个 `asKind`、多一个蛇形名
 * 都会让面板读空。
 */
export interface ViewFrame {
  command: string | null;
  asKind: string;
  exitCode: number | null;
  stdout: string;
  stderr: string;
  truncated: boolean;
  timedOut: boolean;
  note: string | null;
  base: string | null;
  baseKind: string;
}

export interface LogEntry {
  sha: string;
  subject: string;
}

/** `git log --oneline` 一类：短 sha + 空格 + 非空描述。与后端 `classify` 同规则。 */
function looksLikeLog(text: string): boolean {
  const first = text.split(/\r?\n/).find((l) => l.trim() !== "");
  if (first === undefined) return false;
  // 长度 > 8（7 位 sha + 空格 + 至少一个字符），前 7 位是 ascii 十六进制，第 8 位是空格。
  // 大小写都收：后端 `classify` 用 `is_ascii_hexdigit`，这里跟它对齐。
  if (first.length <= 8) return false;
  if (!/^[0-9a-fA-F]{7}/.test(first)) return false;
  if (first[7] !== " ") return false;
  return first.slice(8).trim().length > 0;
}

/**
 * 内容真的像统一 diff 吗。
 *
 * 只认 `diff --git ` 这一处标志。后端 `classify` 还接受「首行以 `--- ` 开头」这条
 * 次信号，这里**故意不跟**：少了它只会把真 diff 降级成纯文本，绝不会把普通文本
 * 误渲染成 diff，方向是安全的。
 */
function looksLikeDiff(text: string): boolean {
  return text.includes("diff --git ");
}

/**
 * 把「声明的原语」解析成「实际的原语」。
 *
 * 声明与内容一致才采信；任何不一致都退化成 `text`——保底永不失手：模型把
 * 普通文本标成 `diff` 时，只会退化成纯文本，不会渲染成一份假的 diff。
 */
export function resolveKind(frame: ViewFrame): ViewKind {
  switch (frame.asKind) {
    case "diff":
      return looksLikeDiff(frame.stdout) ? "diff" : "text";
    case "log":
      return looksLikeLog(frame.stdout) ? "log" : "text";
    case "text":
      return "text";
    default:
      // 未知/空声明：不猜，直接纯文本。
      return "text";
  }
}

/** 拆 `--oneline` 输出；不匹配的行（装饰、噪声、分隔）直接丢弃。 */
export function parseLog(text: string): LogEntry[] {
  const entries: LogEntry[] = [];
  // 按 `\r?\n` 切：CRLF 行尾的 `\r` 若留下，`.` 匹配不到它，整行都会失去匹配（面板空白）。
  for (const line of text.split(/\r?\n/)) {
    const m = /^([0-9a-fA-F]{7,40})\s+(.*)$/.exec(line);
    if (!m) continue;
    entries.push({ sha: m[1], subject: m[2] });
  }
  return entries;
}
