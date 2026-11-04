/**
 * 旁支命令的解析与「这条输入该做什么」的唯一判定（spec §4.1、§9.3）。
 *
 * 为什么要把判定收成一个纯函数：手机的 Stop-only 是一条**安全约束**，不是
 * 一种显示习惯。一旦它散落在「输入框的 disabled」「handleSendClick 的早退」
 * 「runSlash 的守卫」三处，就会长出第四个入口把它绕过去——而这恰恰是旁支
 * 引入时最容易犯的错（旁支需要一个例外，最省事的做法是在每个检查点各开一个
 * 口子）。所以：所有入口都问同一个 `sideInputPolicy`。
 *
 * 三个视图维度的含义：
 * - `view`：当前在看主会话还是旁支。旁支里输入的文字属于旁支。
 * - `running`：**当前视图**的轮次是否在跑（旁支里就是旁支自己）。
 * - `canControl`：本设备是 `Control` 及以上。`Observe` 只能看。
 */

import { parseSlashInput } from "./slash";

/**
 * 一条要执行的命令。
 *
 * 前四种是旁支自己的命令；`command` 是旁支里仍然允许的**状态类**普通命令
 * （`/help`、`/config`、`/usage`）。后者不是旁支专有的，但它必须在旁支作用域
 * 里执行（`/config` 读旁支固定的脱敏配置、`/usage` 只统计旁支），所以同样
 * 由这里分类、而不是溜回父会话的 runSlash。
 *
 * `invalid` 只带给人看的一句话，不带动作。
 */
export type SideCommand =
  | { kind: "open"; question: string }
  | { kind: "back" }
  | { kind: "close" }
  | { kind: "command"; name: string; args: string | null }
  | { kind: "invalid"; message: string };

/**
 * 一条输入的最终动作。
 *
 * - `sendMain`：作为普通消息发给父会话。
 * - `sendSide`：发给旁支。
 * - `executeSideCommand`：执行 `side`/`btw`/`back`/`side-close`。
 * - `stop`：Stop-only 拦截（手机主视图跑着的时候）。
 * - `blocked`：不执行、不清稿，只给出 `reason`。
 */
export type SideInputAction =
  | "sendMain"
  | "sendSide"
  | "executeSideCommand"
  | "stop"
  | "blocked";

export interface SideInputPolicyInput {
  platform: "desktop" | "mobile";
  view: "main" | "side";
  running: boolean;
  attachmentCount: number;
  text: string;
  /** 该宿主是否声明了 `side_conversation_v1`。 */
  supported: boolean;
  /** 本设备能不能下发 Control 类操作（Observe 不能）。 */
  canControl: boolean;
}

export interface SideInputDecision {
  action: SideInputAction;
  reason?: string;
  /** 仅 `executeSideCommand` 有值。 */
  command?: SideCommand;
}

/**
 * 解析以 `/side`、`/btw` 打头的输入。
 *
 * 三个细节都是刻意的：
 *
 * 1. **首个 token 必须精确相等**。`/sidebar` 不是 `/side`，别名也不做前缀匹配
 *    ——它会被当成未知命令（或一条发给模型的普通文本），绝不会悄悄打开旁支。
 * 2. **问题正文只 trim 命令边界**。`/btw   why?  ` 的正文是 `why?`，但如果用户
 *    在问题里写了 `  a  b  ` 这种内部空白，原样保留：那是他要发给模型的内容，
 *    不是命令的一部分。
 * 3. 不带问题也是合法的（打开一个空旁支）。`/side` 与 `/side ` 等价。
 */
export function parseSideCommand(input: string): SideCommand | null {
  const trimmed = input.trimStart();
  if (!trimmed.startsWith("/")) return null;
  // 首 token 里出现两个以上斜杠 = 绝对路径，交给模型。与 `parseSlashInput`
  // 同一条规则：`/side/foo` 是路径，不是「打开旁问并问 foo」。
  const firstSpace = trimmed.search(/\s/);
  const firstToken = firstSpace === -1 ? trimmed : trimmed.slice(0, firstSpace);
  if ([...firstToken].filter((ch) => ch === "/").length >= 2) return null;
  const match = /^\/([^\s/]+)([\s\S]*)$/.exec(trimmed);
  if (!match) return null;
  const name = match[1];
  const rest = match[2];
  switch (name) {
    case "side":
    case "btw":
      return { kind: "open", question: rest.trim() };
    case "back":
      // `/back` 带参数是笔误，不是「回主会话并说点什么」：明确拒绝比静默忽略
      // 更安全——静默忽略会让用户以为自己的话发出去了。
      return rest.trim() === ""
        ? { kind: "back" }
        : { kind: "invalid", message: "/back 不接受参数" };
    case "side-close":
      return rest.trim() === ""
        ? { kind: "close" }
        : { kind: "invalid", message: "/side-close 不接受参数" };
    default:
      return null;
  }
}

/** 旁支里允许执行的非旁支命令（其余一律保留输入并拒绝）。 */
const SIDE_SAFE_COMMANDS = new Set(["help", "config", "usage"]);

/**
 * 判定这条输入该做什么。**所有入口都必须走这里**。
 */
export function sideInputPolicy(input: SideInputPolicyInput): SideInputDecision {
  const sideCommand = parseSideCommand(input.text);
  const slash = sideCommand === null ? parseSlashInput(input.text) : null;
  const isCommand = sideCommand !== null || (slash !== null && slash.kind === "command");

  if (isCommand && input.attachmentCount > 0) {
    // 命令带附件没有意义，而「静默把附件丢掉」会让用户以为发出去了。
    return { action: "blocked", reason: "先处理待发附件" };
  }

  if (sideCommand !== null) return validateSideCommand(sideCommand, input);

  if (slash !== null && slash.kind === "command" && input.view === "side") {
    // 旁支里只留状态类命令。clear/compact/fork/模型切换这些会改父会话或
    // 重建上下文，静默执行是最糟的结果；保留输入、明确拒绝。
    if (!SIDE_SAFE_COMMANDS.has(slash.name)) {
      return {
        action: "blocked",
        reason: `旁问里不能执行 /${slash.name}，先返回主会话`,
      };
    }
    return {
      action: "executeSideCommand",
      command: { kind: "command", name: slash.name, args: slash.args },
    };
  }

  if (input.view === "side") {
    // 旁支运行中可编辑，但按钮是它自己的 Stop。
    return { action: input.running ? "stop" : "sendSide" };
  }
  if (input.platform === "mobile" && input.running) {
    // 手机主视图的既有约束：运行中只有 Stop，普通文字不插话。
    return { action: "stop" };
  }
  return { action: "sendMain" };
}

/**
 * 旁支命令本身的校验。
 *
 * 与普通命令的区别：`open` 在宿主不支持 / Observe 时是**明确不可用**，不是
 * 「静默不执行」——用户点了没反应会以为程序坏了，而这里应当回一句为什么。
 */
function validateSideCommand(
  command: SideCommand,
  input: SideInputPolicyInput,
): SideInputDecision {
  switch (command.kind) {
    case "invalid":
      return { action: "blocked", reason: command.message };
    case "command":
      // 已在调用方按白名单筛过；这里只把它原样交给作用域内执行。
      return { action: "executeSideCommand", command };
    case "back":
      // 返回永远可用：它不改变任何服务端状态。主视图里执行它等于空操作，
      // 由调用方自己决定是否提示。
      return { action: "executeSideCommand", command };
    case "close":
      if (!input.canControl) {
        return { action: "blocked", reason: "只读设备不能关闭旁问" };
      }
      if (input.view !== "side") {
        return { action: "blocked", reason: "当前没有打开的旁问" };
      }
      return { action: "executeSideCommand", command };
    case "open":
      if (!input.supported) {
        return { action: "blocked", reason: "当前宿主不支持旁问" };
      }
      if (!input.canControl) {
        return { action: "blocked", reason: "只读设备不能发起旁问" };
      }
      if (input.view === "side") {
        // 不嵌套：转成主输入会把用户的话发到父会话上，那是更糟的误解。
        return { action: "blocked", reason: "旁问里不能再次发起旁问" };
      }
      // 父会话跑着也能开旁支——这正是它存在的理由。手机上 Stop-only 的既有
      // 约束只管普通文字，不管这条显式命令（spec §9.3「精确例外」）。
      return { action: "executeSideCommand", command };
  }
}
