# Desktop Mid-turn Input Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** 桌面运行中可不中断提交文字及安全命令，手机维持先停止交互。

**Architecture:** 复用 App 既有 turn/interject。命令目录声明运行中安全性，输入框统一提交入口，App 在执行前守卫并返回是否接受。草稿清理绑定 thread 与提交快照。

**Tech Stack:** React 19、TypeScript、Vitest、Testing Library；服务端 Rust 不改。

## Global Constraints

- 在 `.worktrees/desktop-mid-turn-input`、`feat/desktop-mid-turn-input` 分支工作。
- 手机使用既有 isMobile 判定，不改为视口宽度判定；运行中只 Stop、Enter 换行。
- 桌面普通文字立即投递，在下一次可接收输入的模型轮次生效，不取消当前操作。
- 不扩展附件插话协议，不新增自动发送队列。
- 未知/被拒绝命令、失败消息保留草稿；命令绝不提交给模型。
- 提交前 cargo fmt --all 和 just ci；合并后再次 just ci。

## Task 1: 命令运行中策略

**Files:** `desktop/src/lib/slash.ts`、`desktop/src/lib/slash.test.ts`

**Interfaces:** `SlashCommandSpec` 新增 `availableDuringTurn: boolean`；导出
`canRunSlashDuringTurn(name: string): boolean`，仅目录中显式 true 返回 true。

- [x] 添加测试，断言 help/usage/config true、clear/compact/fork/nope false。

```ts
expect(canRunSlashDuringTurn("help")).toBe(true);
expect(canRunSlashDuringTurn("clear")).toBe(false);
expect(canRunSlashDuringTurn("nope")).toBe(false);
```

- [x] 运行 `cd desktop && npm test -- src/lib/slash.test.ts`，确认缺失函数失败。
- [x] 目录六项显式声明安全性，纯 helper 查目录，未知返回 false。

```ts
return SLASH_COMMANDS.some(c => c.name === name && c.availableDuringTurn);
```

- [x] 同命令重跑通过。

## Task 2: 统一输入交互与 App 防护

**Files:** `desktop/src/components/MessageInput.tsx`、对应 `.test.tsx`、
`desktop/src/App.tsx`、`desktop/src/App.test.tsx`

**Interfaces:** `onSlashCommand` 返回 `boolean | Promise<boolean>`，true 表示接受、
false 表示保留草稿；测试桩默认返回 true。`runSlash` await 后只在提交身份未变时
清草稿；App 按 session.turnActive/status 检查策略、执行前返回 false。

- [x] 替换旧“Enter 停止”的断言；新增运行中 Send/Enter 调 onSend 不调 onInterrupt、
手机仅 Stop、空 Enter、IME、Shift+Enter、Stop 在 sending 时可用、建议框可打开。
- [x] 添加命令失败保留草稿、未知命令保留、Enter/Send/选中建议一致、只读命令运行中
执行、危险命令无变更 RPC 且保留草稿的测试。
- [x] App 新增运行中附件拒绝（文档与图片）、方法竞争、会话切换在途回包不误清草稿测试。

```ts
fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter" });
expect(clients[0].requests.map(r => r.method)).toContain("turn/interject");
expect(clients[0].requests.map(r => r.method)).not.toContain("turn/interrupt");
```

- [x] 跑 `npm test -- src/components/MessageInput.test.tsx src/App.test.tsx`，核对失败与目标一致。
- [x] App 命令入口应用 helper：运行中非安全命令给 notice/false；未知命令帮助/false。
允许命令返回 true，已知失败返回 false；fork 在空闲成功时参数仍写新会话草稿。
- [x] MessageInput 将 Enter/Send 调同一提交分派；建议框 Enter 用同一 runSlash；
runSlash 使用 sending 锁并尊重 boolean。裸 `/` 不能误触默认 clear。
- [x] 删除桌面 Enter 的 interrupt 分支；允许桌面忙碌时斜杠建议；手机沿用原守卫。
- [x] 活跃时渲染独立 Stop；`(!turnActive || !isMobile)` 时渲染 Send。Stop disabled
只受 disabled；Send disabled 为空或 sending。输入区送出回包清稿须核对 threadId/value。
- [x] App `send` 在乐观回显前拒绝活跃状态的 files，写可操作错误、返回 false。

```ts
if (files.length > 0 && (session.turnActive || view.status === "running" || view.status === "awaiting_approval")) {
  session.lastError = "当前任务进行中，附件需等待结束或先停止后发送";
  force(v => v + 1);
  return false;
}
```

- [x] 同测试命令重跑至绿；运行 slash/mobileLayout 与相关组件回归。

## Task 3: 文档、验证与集成

**Files:** `docs/project-management/desktop.md`、`docs/project-management/README.md`、
本计划和设计规格。

- [x] 旧的“桌面必须先 Stop”说明替换成新行为，更新活跃插话、命令、Send/Stop 条目。
保持旧设计作为历史记录，文档链接指新设计；只改既有条目则计数不变。
- [x] `cd desktop && npm test -- --maxWorkers=1 --no-file-parallelism && npm run build`，152 个测试文件、2043 条测试通过，TypeScript/Vite 构建通过。两 worker 运行曾在既有 Git Diff 测试等待详情栏处超时；单测复跑及完整单 worker 复跑均通过。
- [x] 审查 git diff，核对设计所有验收项及移动端无变化。
- [x] `cd yi-agent-rs && cargo fmt --all && just ci`，`CARGO_BUILD_JOBS=2 NEXTEST_TEST_THREADS=8` 完整退出 0，包含 3991 条 Rust 测试通过、28 条既有跳过；构建、绑定、脚本与安全门禁全部通过。
- [ ] 精确 git add 本次文件，conventional commit，无 co-author 行。
- [ ] 回主仓库前确认用户已有变更不动；merge --no-ff 本分支，再运行 just ci 和前端验证。
- [ ] 成功后删 worktree/分支；检查没有编译进程使用本 worktree 才回收其产物。

## Self-review

覆盖普通插话、独立停止、安全/危险/未知命令、附件、失败保留、thread 隔离、
手机键盘和布局、方法竞争；不改服务端，不引入队列和无关重构。执行采用本会话
inline，因为没有可用子代理工具。提交集中在全套门禁通过之后，避免违反仓库提交规范。

### 验证环境记录

首次 Rust CI 的 3991 条测试中，两条 `subagent_real_e2e` 夹具测试在 10 秒 daemon
就绪窗口超时。单独复现并用 macOS `sample` 采样，子进程停在 `_dyld_start`、尚未
进入应用初始化（物理内存 112 KB）；另一次延迟启动时夹具目录已因超时删除。
未修改 Rust 代码、测试或超时，完整复跑使用 `CARGO_BUILD_JOBS=2 NEXTEST_TEST_THREADS=2`。
本轮两条夹具测试均约 0.15 秒通过，但三条插件启动/manifest 测试在 15–20 秒窗口超时，
完整结果为 3988/3991 通过；这三条定点复跑全通过（约 3.8、0.65、0.41 秒）。
最终完整确认使用 `CARGO_BUILD_JOBS=2 NEXTEST_TEST_THREADS=8`，不启用失败自动重试；
3991 条 Rust 测试全部通过、28 条按既有标记跳过，整条 CI 结果以实际退出码核验。
