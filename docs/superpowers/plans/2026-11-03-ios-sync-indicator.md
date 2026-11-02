# iOS 同步提示旋转指示器 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为手机聊天顶部的「正在同步…」增加小型旋转圆环，保持原有同步语义与布局。

**Architecture:** 在现有 CacheNotice 内根据 `syncing && !failed` 渲染无状态装饰元素，使用 Tailwind 的 CSS 动画及减少动态效果变体。宿主 App 的显示条件、状态机、RPC、发送限制及空白转录骨架均不修改。

**Tech Stack:** React 19、TypeScript、Tailwind CSS v4、Vitest、Testing Library、Tauri iOS WKWebView。

## Global Constraints

- 设计依据：[设计文档](../specs/2026-11-03-ios-sync-indicator-design.md)。
- 直径 12px，图文间距 6px，约 1 秒匀速旋转一周。
- 文案保持逐字不变，文字不闪烁、不呼吸，不显示百分比或模拟进度。
- 仅 `syncing && !failed` 时渲染圆环；失败优先。
- 保留提示条原有位置、背景、边框、内边距及行高。
- 保留 `role="status"`。圆环设为 `aria-hidden="true"`。
- 使用 `motion-safe:animate-spin`：系统开启减少动态效果时圆环静止。
- 同步完成沿用宿主的卸载行为；不增加最短显示时间或消失定时器。
- 不新增动画库、JavaScript 定时器或跨组件状态。
- 不扩大提示条在桌面端的显示范围，也不改动空白转录区 TranscriptPlaceholder 的骨架动画。
- 所有修改在新 worktree 分支进行，不在 main 直接修改。提交前执行 `cargo fmt --all` 和 `just ci`，合并后再次执行 `just ci`。

---

## 文件职责

| 文件 | 操作 | 职责 |
|---|---|---|
| `desktop/src/components/CacheNotice.tsx` | 修改 | 唯一运行时代码改动；按状态渲染圆环 |
| `desktop/src/components/CacheNotice.test.tsx` | 修改 | 同步／失败／离线及状态切换回归 |
| `desktop/src/App.test.tsx` | 只运行 | 验证同步结束收敛、失败收敛及桌面不出现提示 |
| `docs/project-management/desktop.md` | 修改 | 完成实现后更新已有待办，不新增重复项 |
| `docs/project-management/README.md` | 修改 | 按完成项实际变化更新 desktop 计数 |

### Task 1: 同步圆环、无障碍降级与完整验收

**Files:**
- Modify: `desktop/src/components/CacheNotice.tsx`
- Test: `desktop/src/components/CacheNotice.test.tsx`
- Regression: `desktop/src/App.test.tsx`
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md`

**Interfaces:**
- Consumes: `CacheNotice({ savedAt?: number | null; syncing?: boolean; failed?: boolean })`。
- Produces: 相同组件接口；同步态多一个 `data-testid="sync-indicator"` 的装饰元素，状态文本与原来一致。
- 不新增导出函数、属性、依赖或 RPC。

- [ ] **Step 1: 准备独立工作区与基线。** 从包含本 spec/plan 的最新 main 创建新 worktree（不要复用已清理的文档工作区），按仓库规则准备 Node 环境；在新工作区运行：

```bash
cd desktop
npm ci
npx vitest run src/components/CacheNotice.test.tsx
```

预期现有 4 个测试通过。若基线失败，先记录并定位原因，不把已有问题算入本特性。

- [ ] **Step 2: 添加失败测试。** 在 CacheNotice 测试现有 describe 内加入以下用例，保留原有 4 个测试：

```tsx
it("shows an accessible motion-safe spinner only while syncing", () => {
  render(<CacheNotice syncing />);
  const indicator = screen.getByTestId("sync-indicator");
  expect(indicator.getAttribute("aria-hidden")).toBe("true");
  expect(indicator.className).toContain("size-3");
  expect(indicator.className).toContain("motion-safe:animate-spin");
  expect(indicator.className).not.toMatch(/(^|\s)animate-spin(\s|$)/);
  expect(screen.getByRole("status").textContent).toBe("正在同步…");
});

it("removes the spinner when syncing becomes failed or offline", () => {
  const { rerender } = render(<CacheNotice syncing />);
  expect(screen.getByTestId("sync-indicator")).toBeTruthy();
  rerender(<CacheNotice syncing failed />);
  expect(screen.queryByTestId("sync-indicator")).toBeNull();
  expect(screen.getByRole("status").textContent).toBe("内容加载失败 · 可重试");
  rerender(<CacheNotice savedAt={null} />);
  expect(screen.queryByTestId("sync-indicator")).toBeNull();
  expect(screen.getByRole("status").textContent).toBe("内容未缓存 · 连接断开");
});

it("does not render a spinner on initial failed or offline mounts", () => {
  const { rerender } = render(<CacheNotice failed />);
  expect(screen.queryByTestId("sync-indicator")).toBeNull();
  rerender(<CacheNotice savedAt={Date.now() - 60_000} />);
  expect(screen.queryByTestId("sync-indicator")).toBeNull();
  expect(screen.getByRole("status").textContent).toContain("离线内容");
});
```

- [ ] **Step 3: 证明红灯。**

```bash
cd desktop
npx vitest run src/components/CacheNotice.test.tsx
```

预期新增同步态测试因找不到 `sync-indicator` 失败；不能接受语法或环境错误作为红灯。

- [ ] **Step 4: 最小实现。** 保持 `offlineLabel` 原样。把 CacheNotice 返回值中的提示条替换为以下 JSX，不改 App 的接线：

```tsx
<div
  role="status"
  className="border-b border-line bg-raised px-4 py-1 text-xs text-fg-muted"
>
  {syncing && !failed ? (
    <span className="inline-flex items-center gap-1.5 align-top">
      <span
        data-testid="sync-indicator"
        aria-hidden="true"
        className="size-3 shrink-0 rounded-full border border-current border-r-transparent motion-safe:animate-spin"
      />
      <span>正在同步…</span>
    </span>
  ) : failed ? (
    "内容加载失败 · 可重试"
  ) : (
    offlineLabel(savedAt)
  )}
</div>
```

保留或更新失败优先的现有注释；装饰元素为空，不向 status 文本引入额外字符。外层提示条类名不变，内层 inline-flex 不得撑高既有行盒。

- [ ] **Step 5: 绿灯及前端回归。**

```bash
cd desktop
npx vitest run src/components/CacheNotice.test.tsx
npx vitest run src/App.test.tsx -t 'syncing|catch-up|syncing notice|cold start'
npx tsc --noEmit
npm test
npm run build
```

预期所有命令退出 0。检查 App 筛选确实执行了同步相关测试；全量前端测试还应覆盖现有同步完成移除、失败收敛及桌面路径不出现提示的行为。

- [ ] **Step 6: 浏览器视觉验收。** 通过项目现有浏览器调试能力，在移动视口下触发真实同步态（调试时可用浏览器网络节流，不提交延时代码）。确认：圆环约 1 秒一圈、文字静止、失败后圆环消失、深浅主题可见；记录提示条同步态与离线态的 `getBoundingClientRect().height`，高度应一致。模拟 `prefers-reduced-motion: reduce`，检查圆环计算样式无运行中的动画；恢复 no-preference 后检查旋转恢复。生产构建的 CSS 应含 spin 关键帧和对应 motion-safe 媒体查询。若无法使用真机，交付明确标记 iOS WKWebView 真机待验，不声称已验证。

- [ ] **Step 7: 更新进度。** 将 desktop 模块中本特性的现有 `[ ]` 条目改为 `[x]`，写明 CacheNotice 文件路径及 Step 5 的验证命令，记录真机待验限制；索引 desktop 完成数加 1，总数不变。不要将 spec/plan 入队本身当成动画完成。

- [ ] **Step 8: 提交前全门禁与提交。** 在本工作区设置正确工具 PATH 后执行：

```bash
cd yi-agent-rs
cargo fmt --all
just ci
cd ..
git diff --check
git add desktop/src/components/CacheNotice.tsx desktop/src/components/CacheNotice.test.tsx docs/project-management/desktop.md docs/project-management/README.md
git commit -m "feat: animate mobile syncing notice"
```

`just ci` 的任一道门禁失败均不得忽略；不要提交无关格式化改动或 Co-Authored-By 行。

环境注意：本次文档准备期间，macOS 默认 `/var/folders/.../T` 下的临时脚本在批量测试中出现启动超时；同批监督模块使用短路径临时目录后 87/87 通过。若复现该现象，先创建独立短路径目录，再用 `TMPDIR=/tmp/yi-ios-sync-ci NEXTEST_TEST_THREADS=4 just ci` 重跑完整门禁；目录按实际会话命名，避免与其他任务共用，不跳过测试或放宽超时。

- [ ] **Step 9: 审查与交付。** 加载 requesting-code-review 和 verification-before-completion；检查 diff 仅包含展示、测试与进度文档，无状态机／RPC／桌面显示条件变化。看板卡执行完声明 `awaiting_merge`，不要直接合并。用户确认后通过 `superpowers-kanban merge-request <card-id>` 排队；由持有合并名额的回合执行 `git merge --no-ff`，合并后运行 `cd yi-agent-rs && just ci` 和前端 Step 5 命令，通过后清理该卡自己的 worktree/分支并声明完成。不要推送远程，除非用户另行要求。

## 计划自检

- 视觉尺寸、间距、旋转速度、静态文字和高度约束：Step 4、6。
- 同步／失败优先／离线／状态切换：Step 2–5。
- 减少动态效果与读屏：Step 2、4、6。
- 不扩大桌面范围、不改发送与同步逻辑：Step 4、5、9。
- 进度文档、完整门禁、看板合并流程：Step 7–9。
