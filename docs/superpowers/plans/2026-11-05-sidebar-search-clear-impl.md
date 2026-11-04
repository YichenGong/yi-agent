# Desktop 侧栏搜索框清除按钮修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 侧栏非空查询只显示一个自定义清除按钮，保留清空、搜索语义和移动端样式。

**Architecture:** 在真实侧栏输入框添加专用类及右侧留白，使用局部 WebKit 伪元素规则隐藏原生按钮。沿用现有查询状态、clearQuery 和自定义按钮，不抽取组件、不修改 SearchBar。组件测试锁住 DOM/状态，CSS 文本测试锁住规则作用域，真实 Desktop WKWebView 验收补足原生控件渲染证据。

**Tech Stack:** Tauri 2 / macOS WKWebView、React 19、TypeScript、Tailwind CSS v4、Vitest、Testing Library。

## Global Constraints

- 设计真源：[spec](../specs/2026-11-05-sidebar-search-clear-design.md)。本计划尚未执行；用户已授权审查通过后正式入队，实施由看板调度，不由文档会话抢跑。
- 保留 `type="search"`、`aria-label="搜索会话"`、placeholder、受控 value 和全部事件处理。
- 显示条件保持 `query !== ""`，纯空格同样视为非空。
- 保留按钮 `type="button"`、`aria-label="清除搜索"`、title 与现有定位和颜色。
- 点击仍调用现有 `clearQuery()`，Escape 路径不变；不添加独立状态或第二套清空逻辑。
- 不修改 Cmd+F 查找栏。不全局隐藏其他搜索框的原生清除控件。
- 不抽取通用组件，不新增依赖、协议、后端或持久化变更。
- 不借此次修复重新设计按钮外观或定位。不更改点击后的焦点策略。
- 保留移动端 `font-size: 16px`、`min-height: 44px`、`margin-top: 0.5rem` 规则。
- 不在 main 修改或提交。文档先从 `docs/sidebar-search-clear-spec` 合入 main，再以 main 中的路径入队；看板执行使用它分配的 `kanban/*` 分支和当前卡片 worktree，不切换回文档分支，不另建嵌套 worktree。
- 每次 commit 前执行 `cd yi-agent-rs && cargo fmt --all && just ci`。合并 main 后再运行 `just ci`。未通过门禁不得提交；不得绕过 hook。
- 完成状态依赖真实 macOS Desktop 验收，不以 CSS 文本检查或 jsdom 通过代替。
- 真机验收不替换正式版；独立 `YI_AGENT_HOME`、空 `YI_AGENT_RELAY`、独立 bundle identifier，使用临时项目，不读取正式会话，不调用真实 LLM。
- 这是一个小型修复，测试、实现、验收和管理记录在门禁通过后作为一个原子提交交付，避免为拆分微小提交反复运行全量 CI。不要在失败的红测阶段提交。
- 看板入队传递的是文件路径，不是文档快照。入队前两份文档须已合入 main；卡片开始时应能在自己的工作树读到相同相对路径，不依赖文档来源 worktree 的未提交内容。文档合并与正式入队由本轮交接完成，不是实施任务。
- 需要用户操作或设备导致无法继续时，声明 `needs_you`；依赖/权限等环境故障声明 `failed` 并记录原因；只有计划验收与提交完成才声明 `awaiting_merge`。不得仅因文档写完或前端测试通过而标记 `done`。
- 所有正向命令块由 Bash 执行并启用失败即停；红测块显式捕获预期非零码。不同工具调用不共享 shell 变量，验收上下文仅通过当前工作树忽略目录 `.artifacts/sidebar-search-clear/context.env` 传递，不写入密钥。

---

## 文件职责

| 文件 | 操作 | 职责 |
|---|---|---|
| `desktop/src/components/ThreadSidebar.tsx` | 修改 | 仅增加搜索输入框专用类和右侧留白 |
| `desktop/src/index.css` | 修改 | 局部隐藏原生清除按钮 |
| `desktop/src/components/ThreadSidebar.test.tsx` | 修改 | 真实组件的结构、非空定义和清空回归 |
| `desktop/src/sidebarSearchCss.test.ts` | 新增 | 读取真实 index.css，检查隐藏声明及选择器作用域 |
| `docs/project-management/desktop.md` | 修改 | 记录实现和实际验收证据，未验保持未完成 |
| `docs/project-management/README.md` | 修改 | 链接及一级条目计数 |
| 本 spec 和 plan | 修改 | 按实际情况记录执行状态，不预填通过结果 |

现有 `mobileLayout.test.ts`、`fontScaleCss.test.ts`、`SearchBar.test.tsx` 仅运行回归，不预设需要改动。

## Task 1：用回归测试驱动最小修复

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.test.tsx`，`describe("ThreadSidebar 搜索")` 内。
- Create: `desktop/src/sidebarSearchCss.test.ts`。
- Modify: `desktop/src/components/ThreadSidebar.tsx`，`const searchBar` 内，基线约第 1723–1747 行。
- Modify: `desktop/src/index.css`，导入/插件声明之后、语义 token 之前新增未分层规则。

**Interfaces:**
- Consumes: 已有 `renderSidebar(overrides?: Partial<ComponentProps<typeof ThreadSidebar>>)`、`searchBox(): HTMLInputElement` 和 `clearQuery(): void`，均不修改签名。
- Produces: 侧栏 input 的 `.thread-search-input` 和 `pr-8` 类；唯一的 `.thread-search-input::-webkit-search-cancel-button` CSS 规则。无新增公共函数或状态。

- [ ] **Step 1：确认执行位置与基线。**

在看板分配的当前卡片 worktree 根执行，分支必须为 `kanban/*`。spec/plan 已先行合入 main，应随新卡片工作树一起存在；不切换到旧文档 worktree，也不从主检出复制未提交代码：

```bash
set -euo pipefail
pwd
BRANCH="$(git branch --show-current)"
case "$BRANCH" in
  kanban/*) ;;
  *) printf 'Unexpected implementation branch: %s\n' "$BRANCH" >&2; exit 1 ;;
esac
git status --short
test -f docs/superpowers/specs/2026-11-05-sidebar-search-clear-design.md
test -f docs/superpowers/plans/2026-11-05-sidebar-search-clear-impl.md
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd desktop
npm ci
npm test -- src/components/ThreadSidebar.test.tsx src/mobileLayout.test.ts src/fontScaleCss.test.ts src/components/SearchBar.test.tsx
cd ..
```

续跑时核对本地已勾选步骤并保留已有改动，不使用 reset/checkout 清理它们。若卡片工作树缺少已合入 main 的文档，停止并以 `needs_you` 回报，禁止回退到主检出修改或猜测其他来源文件。

预期基线通过；本轮文档阶段没有运行这组命令，不引用调查时两条测试代替本次基线。若依赖安装或测试失败，先按 systematic-debugging 分清环境问题与基线缺陷；不要修改无关代码掩盖问题。

- [ ] **Step 2：在现有搜索 describe 中添加组件回归。**

复用文件顶部现有 imports、`renderSidebar` 和 describe 内的 `searchBox`。添加如下用例，保留已有点击清空、Escape 和折叠恢复测试：

```tsx
it.each([
  { name: "桌面", isMobile: false },
  { name: "手机", isMobile: true },
])("$name 搜索框保留 search 语义及独立清除控件样式", ({ isMobile }) => {
  renderSidebar({ isMobile });
  const box = searchBox();
  expect(box.type).toBe("search");
  expect(box.classList.contains("thread-search-input")).toBe(true);
  expect(box.classList.contains("pr-8")).toBe(true);
});

it.each(["beta", "   "])("非空查询 %j 仅渲染一个自定义清除按钮", (value) => {
  renderSidebar();
  expect(screen.queryAllByRole("button", { name: "清除搜索" })).toHaveLength(0);
  fireEvent.change(searchBox(), { target: { value } });
  const buttons = screen.getAllByRole("button", { name: "清除搜索" });
  expect(buttons).toHaveLength(1);
  expect(buttons[0].getAttribute("type")).toBe("button");
  expect(buttons[0].getAttribute("title")).toBe("清除搜索");
  expect((buttons[0] as HTMLButtonElement).disabled).toBe(false);
  fireEvent.click(buttons[0]);
  expect(searchBox().value).toBe("");
  expect(screen.queryAllByRole("button", { name: "清除搜索" })).toHaveLength(0);
  expect(screen.queryByText("alpha-thread")).not.toBeNull();
});

it.each(["button", "escape"] as const)("%s 清空后保持搜索前的分组折叠状态", (via) => {
  const { container } = renderSidebar();
  fireEvent.click(container.querySelector('[aria-label="Collapse"]')!);
  expect(container.textContent).not.toContain("alpha-thread");
  fireEvent.change(searchBox(), { target: { value: "alpha" } });
  expect(container.textContent).toContain("alpha-thread");
  if (via === "button") {
    fireEvent.click(screen.getByRole("button", { name: "清除搜索" }));
  } else {
    fireEvent.keyDown(searchBox(), { key: "Escape" });
  }
  expect(searchBox().value).toBe("");
  expect(container.textContent).not.toContain("alpha-thread");
});
```

该数量用例只验证自定义 DOM，不证明原生按钮消失。结构用例预期在当前实现失败，非空与折叠用例可以在旧实现通过，它们是保护已有行为的回归而不是视觉问题的直接红测。代码沿用普通 Vitest matcher，不引入 jest-dom。

- [ ] **Step 3：创建 CSS 回归文件，内容如下。**

```ts
/** @vitest-environment node */
import { describe, expect, it } from "vitest";
// @ts-ignore -- node 内置模块;本包不依赖 @types/node
import { readFileSync } from "node:fs";

const dir = String((import.meta as unknown as { dirname: string }).dirname);
const css = () => readFileSync(`${dir}/index.css`, "utf8")
  .replace(/\/\*[\s\S]*?\*\//g, "");
const selector = ".thread-search-input::-webkit-search-cancel-button";

function cancelRules() {
  return [...css().matchAll(/([^{}]+)\{([^{}]*)\}/g)]
    .map((match) => ({
      selector: match[1].slice(match[1].lastIndexOf(";") + 1).trim(),
      body: match[2],
    }))
    .filter((rule) => rule.selector.includes("::-webkit-search-cancel-button"));
}

describe("sidebar search clear CSS", () => {
  it("scopes the only native clear override to the sidebar input", () => {
    expect(cancelRules().map((rule) => rule.selector)).toEqual([selector]);
  });

  it("hides the native clear control explicitly", () => {
    const body = cancelRules().find((rule) => rule.selector === selector)?.body ?? "";
    expect(body).toMatch(/(?:^|;)\s*-webkit-appearance:\s*none\s*;/);
    expect(body).toMatch(/(?:^|;)\s*appearance:\s*none\s*;/);
    expect(body).toMatch(/(?:^|;)\s*display:\s*none\s*;/);
  });
});
```

此轻量正则只检查当前简单规则，非通用 CSS 解析器；新增规则必须是独立、未分层的普通声明块，不利用复杂嵌套绕过检查。去除注释避免注释里的示例触发假阳性；选择器前缀取最后一个分号之后的内容，避免位于文件开头的 `@import` / `@plugin` 无花括号语句被并入第一个规则的选择器。复审必须按本计划的实际插入位置（imports/plugin 之后）验证解析结果，不只测试把规则追加到文件末尾。

- [ ] **Step 4：运行红测，确认失败原因。**

```bash
set -euo pipefail
cd desktop
set +e
npm test -- src/components/ThreadSidebar.test.tsx src/sidebarSearchCss.test.ts
RED_STATUS=$?
set -e
printf 'red-test exit status: %s\n' "$RED_STATUS"
test "$RED_STATUS" -ne 0
cd ..
```

预期：桌面/手机结构用例因专用类和 `pr-8` 尚不存在失败；CSS 两项因规则不存在失败。不得因导入错误、TypeScript 错误或测试未被收集而把红测算作有效证据。

- [ ] **Step 5：实施唯一的产品变更。**

只将侧栏搜索 input 的 className 改为：

```tsx
className="thread-search-input w-full rounded-md border border-line bg-surface px-2 pr-8 py-1.5 text-sm text-fg placeholder:text-fg-faint focus:border-fg-subtle focus:outline-none"
```

在 `index.css` 的 imports/plugin 之后增加：

```css
/* 侧栏保留自定义清除按钮，避免与 WebKit 原生搜索清除控件重复。 */
.thread-search-input::-webkit-search-cancel-button {
  -webkit-appearance: none;
  appearance: none;
  display: none;
}
```

不改按钮、输入类型、事件或状态逻辑，不对 SearchBar 添加此类。

- [ ] **Step 6：运行绿测和前端回归。**

```bash
set -euo pipefail
cd desktop
npm test -- src/components/ThreadSidebar.test.tsx src/sidebarSearchCss.test.ts src/mobileLayout.test.ts src/fontScaleCss.test.ts src/components/SearchBar.test.tsx
npm test -- --maxWorkers=2 --no-file-parallelism
npm run build
cd ..
git diff --check
git diff -- desktop/src/components/ThreadSidebar.tsx desktop/src/index.css
```

预期：新增及既有测试、tsc 和生产构建通过。检查产物 CSS 中隐藏规则仍受专用类约束，`pr-8` 最终生效；真实布局留给 Task 2 验证。失败时先定位，不扩大修复范围。

- [ ] **Step 7：完成任务内审查，不立即提交。**

检查：产品源码仅预期两处、没有全局隐藏规则、没有 SearchBar/移动字号变更、测试不把 DOM 数量等同原生按钮数量。提交在 Task 2 完成真实验收及全量门禁后统一进行。

## Task 2：真实 WKWebView 验收、证据登记与交付

**Files:**
- Modify: `docs/project-management/desktop.md`、`docs/project-management/README.md`。
- Modify: 本 plan 的 checkbox 与 spec 的执行状态（仅按真实结果更新）。
- Runtime only: 临时验收目录内的 app 副本、测试项目和实例状态，不提交生成产物。

**Interfaces:**
- Consumes: Task 1 的真实组件和已构建 CSS；专用类与 search 类型不变。
- Produces: 可核对的 macOS Desktop 验收记录、测试/构建/门禁结果及一个 conventional commit。没有真机证据时只产生“待验”的记录，不标记已修复。

- [ ] **Step 1：构建当前 worktree 的独立 app，明确此步骤只在获准执行计划后进行。**

在 worktree 根执行，不使用主仓库旧 sidecar 冒充当前产物：

```bash
set -euo pipefail
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
WT="$(pwd -P)"
cd "$WT/desktop"
npm run sidecar:release
npm run build
npm run tauri -- build --bundles app
cd "$WT"
TAURI_TARGET="$(cd desktop/src-tauri && cargo metadata --format-version 1 --no-deps | node -e 'let s="";process.stdin.on("data",x=>s+=x);process.stdin.on("end",()=>console.log(JSON.parse(s).target_directory))')"
PRODUCT="$(node -e 'console.log(JSON.parse(require("fs").readFileSync("desktop/src-tauri/tauri.conf.json","utf8")).productName)')"
SOURCE_APP="$TAURI_TARGET/release/bundle/macos/$PRODUCT.app"
test -d "$SOURCE_APP"
mkdir -p "$WT/.artifacts/sidebar-search-clear"
git check-ignore -q "$WT/.artifacts/sidebar-search-clear/context.env"
printf 'SOURCE_APP=%q\n' "$SOURCE_APP" > "$WT/.artifacts/sidebar-search-clear/context.env"
printf 'source app: %s\n' "$SOURCE_APP"
```

sidecar 脚本会生成 externalBin 并验证 build id，避免 worktree 缺二进制导致 Tauri 构建失败。先构建前端再打包，避免 devUrl 白屏。目标路径从 cargo metadata 解析，不假定全局 `CARGO_TARGET_DIR` 未设置。若编译长时间运行，使用 managed process 工具监控；不并发启动竞争同一 target 的 cargo 构建。

- [ ] **Step 2：复制 bundle 并隔离身份与状态。**

以下命令在卡片 worktree 根启动一个新 Bash，通过上一步生成的忽略目录上下文恢复产物路径。在独立临时目录操作，不安装到 `/Applications`：

```bash
set -euo pipefail
WT="$(pwd -P)"
CONTEXT="$WT/.artifacts/sidebar-search-clear/context.env"
source "$CONTEXT"
test -d "$SOURCE_APP"
VERIFY_DIR="$(mktemp -d "${TMPDIR:-/tmp}/yi-search-clear.XXXXXX")"
APP="$VERIFY_DIR/yi-agent-search-clear.app"
INSTANCE="$VERIFY_DIR/instance"
mkdir -p "$INSTANCE" "$VERIFY_DIR/project"
/usr/bin/ditto "$SOURCE_APP" "$APP"
P="$APP/Contents/Info.plist"
/usr/libexec/PlistBuddy -c 'Set :CFBundleIdentifier com.gongyichen.desktop.searchclear' "$P"
/usr/libexec/PlistBuddy -c 'Set :CFBundleName yi-agent-search-clear' "$P"
/usr/libexec/PlistBuddy -c 'Set :CFBundleDisplayName yi-agent-search-clear' "$P"
/usr/libexec/PlistBuddy -c 'Delete :LSEnvironment' "$P" 2>/dev/null || true
/usr/libexec/PlistBuddy -c 'Add :LSEnvironment dict' "$P"
/usr/libexec/PlistBuddy -c "Add :LSEnvironment:YI_AGENT_HOME string $INSTANCE" "$P"
EXECUTABLE="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$P")"
codesign --force --sign - "$APP/Contents/MacOS/yi-agent"
codesign --force --sign - "$APP/Contents/MacOS/$EXECUTABLE"
codesign --force --sign - "$APP"
codesign -v "$APP"
printf 'SOURCE_APP=%q\nVERIFY_DIR=%q\nAPP=%q\nINSTANCE=%q\nEXECUTABLE=%q\n' \
  "$SOURCE_APP" "$VERIFY_DIR" "$APP" "$INSTANCE" "$EXECUTABLE" > "$CONTEXT"
printf 'VERIFY_DIR=%s\nAPP=%s\nINSTANCE=%s\nEXECUTABLE=%s\n' "$VERIFY_DIR" "$APP" "$INSTANCE" "$EXECUTABLE"
```

不得复制 `~/.yi-agent` 的 relay、密钥或会话配置。上下文文件仅保存本次生成的路径，不含密钥；不同工具调用都从当前卡片 worktree 显式读取它，不假定 shell 变量跨调用保留。验证配置中的 YI_AGENT_HOME 是绝对路径，不含未展开的 `~`。验收进程通过启动命令显式将 YI_AGENT_RELAY 设为空，不依赖 PlistBuddy 对空字符串的解析。

- [ ] **Step 3：直接启动隔离 Desktop，不用 open -a。**

人工终端可前台执行：

```bash
set -euo pipefail
source "$(pwd -P)/.artifacts/sidebar-search-clear/context.env"
env YI_AGENT_HOME="$INSTANCE" YI_AGENT_RELAY="" \
  "$APP/Contents/MacOS/$EXECUTABLE"
```

代理执行时用 `process_start`，cwd 设为当前卡片 worktree 根，command 使用上面的完整 Bash 启动命令（包含 source），记录 process id；不要用会被工具回收的临时 bash 后台进程。检查该实例确实生成独立状态目录、sidecar 参数没有 `--relay`，进程归属是本 app 副本。只在临时 `project` 目录新建空测试会话，不打开真实项目，不提交聊天消息或调用模型；测试查询本身无需 LLM 或 API key。若必须配置模型才能创建会话，不录入真实密钥，先记录阻塞并处理测试数据准备，不能改用正式数据。

- [ ] **Step 4：执行 spec §7 完整验收矩阵并记录证据。**

验收尺寸至少为默认侧栏 256px 和最窄侧栏 200px。深色/浅色 × 小/标准/大字号分别检查非空查询，至少附一份修复后截图及超长文本截图；检查右留白实际生效而不是只看 className。

逐项记录空输入、非空、纯空格、失焦重聚焦、点击清空、Escape、折叠恢复、键盘按钮可达及 Cmd+F 无回归。Tab 验收须注明 macOS 完整键盘导航前提；不擅自改用户系统设置。

可在尚未修复的基线独立构建中收集两个 X 的对照证据，但不为此修改或切换当前 worktree 的未提交改动。若无旧版本视觉证据，如实注明，不能把源码推断写成已现场复现。

移动端补充回归按 spec 执行；无设备则记录“移动端真机未验，静态和 DOM 回归通过/失败的实际结果”，不把该项写成真机通过。macOS Desktop 必验项未通过时不得标记功能完成。

- [ ] **Step 5：结束隔离验收，先退出 Desktop 父进程。**

优先在隔离窗口 Cmd+Q；代理用对应 managed process id 停止本次父进程，确认该 app 副本的 sidecar 不再运行。只处理能确认属于本次 app 的 PID，禁止按 `yi-agent` 名称批量杀进程。不要先单独杀 sidecar，以免被监督循环重启。先保留日志和截图，未确认全部进程退出前不删除 VERIFY_DIR。

- [ ] **Step 6：同步文档和实际完成状态。**

在 `desktop.md` 的本条记录中写入：实际代码位置、命令结果、WKWebView 的系统/应用版本、主题/字号矩阵结果、未验平台。只有 spec §8 的必要条件都满足后才将 `[ ]` 改为 `[x]`。

本次文档审查时一级条目共 134 项，其中 `[x]` 为 121、`[ ]` 为 13；统计忽略缩进的子条目，不把它们重复计入。执行时可能已有其他分支合入，重新计算，不硬编码加一：

```bash
set -euo pipefail
node --input-type=module <<'JS'
import fs from 'node:fs';
const text = fs.readFileSync('docs/project-management/desktop.md', 'utf8');
const done = [...text.matchAll(/^- \[x\] /gm)].length;
const total = [...text.matchAll(/^- \[(?:x| |-)\] /gm)].length;
console.log(`desktop: ${done} / ${total}`);
JS
```

将输出同步到 README 的 desktop 行，保留 spec 和 plan 链接。计划 checkbox 仅按实际完成勾选；未运行的门禁或未执行的真机检查不得预填成功。

- [ ] **Step 7：运行项目门禁并审查 diff。**

```bash
set -euo pipefail
cd yi-agent-rs
cargo fmt --all
just ci
cd ..
git diff --check
git status --short
git diff -- desktop/src/components/ThreadSidebar.tsx desktop/src/index.css desktop/src/components/ThreadSidebar.test.tsx docs/project-management/desktop.md docs/project-management/README.md
```

另外读取新增 CSS 测试、spec、plan 全文，因为未跟踪文件不会出现在普通 `git diff` 中。确认格式化没有引入无关代码改动；若有，定位原因，不悄悄把无关修改带入提交。所有门禁通过后才进行下一步。

- [ ] **Step 8：提交一个原子修复，不自动合并或推送。**

```bash
set -euo pipefail
git add desktop/src/components/ThreadSidebar.tsx desktop/src/index.css \
  desktop/src/components/ThreadSidebar.test.tsx desktop/src/sidebarSearchCss.test.ts \
  docs/project-management/desktop.md docs/project-management/README.md \
  docs/superpowers/specs/2026-11-05-sidebar-search-clear-design.md \
  docs/superpowers/plans/2026-11-05-sidebar-search-clear-impl.md
git diff --cached --check
git diff --cached --stat
git commit -m "fix(desktop): avoid duplicate sidebar search clear controls"
```

首行小于 72 字符，不添加 Co-Authored-By。提交后读取实际 commit 和 `git status --short` 确认状态；如门禁或真机验收受阻，不提交、不宣称修复完成，报告具体阻塞。

看板卡片提交及验收完成后，用下列命令回报 `awaiting_merge`，不自行执行 git merge。需要用户手动操作/设备时将末行状态替换为 `needs_you --detail "需要用户协助完成真实 WKWebView 验收"`；遇到依赖/权限等环境故障时使用 `failed --detail "实际故障原因"` 并如实填写，不冒充已完成。

```bash
set -euo pipefail
PROJECT_ROOT="$(cd "$(git rev-parse --git-common-dir)/.." && pwd -P)"
test -n "${SUPERPOWERS_KANBAN_CARD:-}"
superpowers-kanban state --self awaiting_merge --state-dir "$PROJECT_ROOT/.yi-agent/superpowers-kanban"
```

`SUPERPOWERS_KANBAN_CARD` 缺失则先通过 CLI list 核对实际卡号，再显式用该卡号回报，禁止猜号。用户确认合并后只执行 `merge-request` 排队；等待 daemon 把持有合并名额的回合送回该卡片开发会话，才按那轮授权 `--no-ff` 合并并在 main 再跑 `just ci`。未合并前不删 worktree；未经确认不执行 cargo clean，不清理其他工作任务的产物。

## Plan 自检与覆盖映射

| Spec 要求 | 计划覆盖 |
|---|---|
| 侧栏局部隐藏、search 语义和右留白 | Task 1 Steps 2–6 |
| 非空/纯空格、按钮属性、点击/Escape/折叠状态 | Task 1 Step 2 与现有回归 |
| 不影响 Cmd+F、移动字号与布局 | Task 1 Step 6；Task 2 Step 4 |
| CSS 隐藏声明及全局/逗号分组误用防护 | Task 1 Steps 3–4 |
| jsdom 不替代真实 WebView | Task 2 Steps 1–4 |
| 临时实例身份、状态、项目隔离及进程清理 | Task 2 Steps 1–5 |
| 未验不冒充通过、完成判据及计数 | Task 2 Steps 6–8 |
| 提交前/合并后门禁及回滚边界 | Global Constraints、Task 2 Steps 7–8；回滚遵循 spec §9 |

自检：没有新增公共类型或跨任务函数签名；代码只引用已核对的现有测试 helper；拟新增文件有明确内容和命令；用户已授权正式入队，实际代码修改由看板调度执行。真实平台验收若受阻即保持未完成，不以计划文档替代证据。入队前复审已消除固定文档分支、未提交文档依赖、shell 变量跨调用及合并名额规则冲突，并修正文件首部 @import/@plugin 对 CSS 规则选择器提取的影响。
