# Desktop 侧栏搜索框重复清除按钮修复设计

日期：2026-11-05
状态：经本会话两轮代码与验收约束核对，未发现阻塞性设计问题；已据用户授权编写实施计划。尚未实现或验证修复。
分支：`docs/sidebar-search-clear-spec`

## 1. 背景与证据

用户反馈 Desktop 搜索框输入内容后出现两个用于清空内容的 X。本设计限定为侧栏“搜索会话”输入框，不包含 Cmd+F 会话内查找栏。

调查基线：`6fb5507e`。`desktop/src/components/ThreadSidebar.tsx` 的 `searchBar` 同时包含：

- `type="search"` 的受控输入框（调查时第 1725 行）。WebKit 可为非空搜索输入框提供原生清除控件。
- 当 `query !== ""` 时渲染的自定义 `×` 按钮，带 `aria-label="清除搜索"`（调查时第 1736–1745 行）。
- 两条清空路径：按钮点击及 Escape 均调用 `clearQuery()`，后者执行 `setQuery("")`。

`desktop/src/index.css` 未屏蔽 `::-webkit-search-cancel-button`。两个清除控件并存是重复 X 的来源，不是组件重复挂载。

浏览器行为参考：[MDN：::-webkit-search-cancel-button](https://developer.mozilla.org/en-US/docs/Web/CSS/::-webkit-search-cancel-button)。该伪元素为非标准浏览器扩展，适用于本次 WebKit 原生控件；不据此宣称所有浏览器行为一致。

调查时现有“清除按钮仅在有内容时出现，点击后恢复完整列表”和“Escape 清空查询”两项测试通过。它们运行于 jsdom，不绘制原生控件，不能证明真实 Desktop 只显示一个 X。尚未进行修复后的真实 WKWebView 验收。

## 2. 目标与非目标

### 目标

- 侧栏查询为空时无清除按钮；非空时只显示一个自定义清除按钮。
- 保留搜索语义、现有查询状态、点击及 Escape 清空行为、列表恢复与折叠恢复行为。
- 为自定义按钮预留输入框右侧空间，长文本和光标不被按钮遮挡。
- 保留“清除搜索”的无障碍名称及已有主题、字号、移动端样式。

### 非目标

- 不修改 Cmd+F 查找栏。其关闭按钮用于关闭查找栏，与清空文本不是同一操作。
- 不全局隐藏其他搜索框的原生清除控件。
- 不调整搜索算法、数据流、焦点策略、IME 行为或分组逻辑。
- 不抽取通用组件，不新增依赖、协议、后端或持久化变更。
- 不借此次修复重新设计按钮外观或定位。

## 3. 方案比较

| 方案 | 收益 | 代价或风险 | 选择 |
|---|---|---|---|
| 局部隐藏原生按钮，保留自定义按钮和 search 类型 | 保持行为、语义与样式；影响范围最小 | 需要浏览器专用伪元素规则和真实 WebView 验收 | 推荐并作为本 spec 的设计 |
| 删除自定义按钮，仅依赖原生按钮 | 减少自定义 DOM | 原生外观、显示条件和交互依赖 WebView，现有自定义按钮测试及无障碍入口需调整 | 不采用 |
| 改成 type="text" | 不产生原生搜索清除控件 | 改变搜索语义，现有移动端 input[type="search"] 规则失配 | 不采用 |

## 4. 详细设计

### 4.1 输入框

在 `ThreadSidebar.tsx` 的侧栏搜索输入框添加专用类 `thread-search-input`。

- 保留 `type="search"`、`aria-label="搜索会话"`、placeholder、受控 value 和全部事件处理。
- 保留 `px-2` 的左侧间距，增加 `pr-8`，为右侧自定义按钮留出空间。
- 若真实 WebView 验收发现大字号下仍存在遮挡，仅在本输入框内调整预留空间；不得以缩小字号或移除清除按钮规避问题。

### 4.2 样式

在 `desktop/src/index.css` 中添加未分层、受专用类约束的规则：

```css
.thread-search-input::-webkit-search-cancel-button {
  -webkit-appearance: none;
  appearance: none;
  display: none;
}
```

规则同时适用于侧栏复用该输入框的移动端路径。不得改成全局 `input[type="search"]` 规则，也不额外屏蔽 `::-webkit-search-decoration` 等无关装饰。

### 4.3 自定义按钮及状态

- 显示条件保持 `query !== ""`，纯空格同样视为非空。
- 保留按钮 `type="button"`、`aria-label="清除搜索"`、title 与现有定位和颜色。
- 点击仍调用现有 `clearQuery()`，Escape 路径不变；不添加独立状态或第二套清空逻辑。
- 清空后通过现有 React 状态更新恢复列表和分组折叠视图，不增加副作用。
- 保留键盘 Tab 可达性，不更改点击后的焦点策略。

### 4.4 兼容性与错误处理

保留 `type="search"`，使 `[data-mobile="true"] .app-sidebar input[type="search"]` 的字号、最小高度和间距规则继续生效。样式不引入运行时异常路径，无需新增错误提示。非 WebKit/Blink 环境可能忽略该伪元素规则，不视为已验证跨平台；后续若覆盖其他 Desktop WebView，须分别核对原生控件行为，不能仅凭 macOS 验收宣称通过。

## 5. 改动边界

预计实现涉及：

- `desktop/src/components/ThreadSidebar.tsx`：专用类和右侧留白。
- `desktop/src/index.css`：局部隐藏原生控件。
- `desktop/src/components/ThreadSidebar.test.tsx`：保留行为用例，增加输入框结构约束。
- CSS 回归测试：沿用现有 CSS 文本检查模式，可新增 `desktop/src/sidebarSearchCss.test.ts`。
- `docs/project-management/desktop.md` 与索引：记录未完成项，实际修复验收后更新。

本轮仅落盘 spec 和项目管理记录；上述产品代码及测试改动均留待实施。

## 6. 自动化验证要求

### 组件测试

1. 输入框仍为 search 类型且具有专用类及右侧预留空间。
2. 初始空查询没有“清除搜索”按钮；输入普通文本或纯空格后恰有一个自定义按钮。
3. 点击按钮后输入为空、按钮消失、完整列表恢复。
4. Escape 清空行为及搜索结束后的分组折叠恢复行为不变。

### CSS 回归

检查专用选择器具有隐藏原生清除控件的声明，且没有新增全局隐藏规则。静态测试先移除 CSS 注释，再收集所有包含 `::-webkit-search-cancel-button` 的规则选择器；当前文件预期仅有一个选择器，且必须严格等于 `.thread-search-input::-webkit-search-cancel-button`，从而拒绝通用 input 规则、全局规则及混入其他选择器的逗号分组。分别检查三条隐藏声明。结合现有移动布局测试，确保 search 类型规则仍匹配。测试不得把 DOM 中只有一个 button 当成原生控件已消失的证据。此检查是本修复的局部回归约束，不新增通用 CSS 解析器或依赖。

### 实施阶段命令

在实施 worktree 内运行：

```bash
cd desktop
npm ci
npm test -- src/components/ThreadSidebar.test.tsx src/sidebarSearchCss.test.ts src/mobileLayout.test.ts src/fontScaleCss.test.ts src/components/SearchBar.test.tsx
npm test -- --maxWorkers=2 --no-file-parallelism
npm run build
```

`sidebarSearchCss.test.ts` 是拟新增测试，当前尚不存在；以上命令是实施验收要求，不是本轮已执行结果。提交前仍须按项目要求在 `yi-agent-rs/` 执行 `cargo fmt --all` 和 `just ci`，合并后再次执行 `just ci`。

## 7. 真实 WebView 验收

macOS Desktop WKWebView 是本次双 X 的必验环境。记录系统与应用版本、主题、字号和验收结果；浏览器或 jsdom 通过不能替代它。

验收只能运行实施 worktree 构建的隔离实例：设置独立 `YI_AGENT_HOME`，清空 `YI_AGENT_RELAY`，不复制正式实例配置；若使用 app bundle，复制到独立临时验收目录，路径记录在 worktree 的忽略目录，赋予独立 bundle identifier 和名称，修改后按 sidecar → desktop → app 重签。直接运行其 `Contents/MacOS/` 中由 `CFBundleExecutable` 指定的可执行文件，不使用 `open -a`，不替换 `/Applications` 正式版。仅使用隔离实例新建的临时项目及测试会话，不打开正式项目或真实会话，不调用真实 LLM。完整命令由实施计划提供。

Tab 导航验收时，先确认 macOS 已开启允许 Tab 到达按钮的系统/应用完整键盘导航设置；设置未启用导致按钮无法 Tab 到达，不能直接归因于本次修复。需要调整设置时由用户操作，验收记录注明该前提。

| 场景 | 通过条件 |
|---|---|
| 空输入 | 无清除按钮 |
| 普通文本输入并聚焦 | 仅有一个可见、可点击的自定义清除按钮 |
| 失焦再聚焦 | 不出现第二个 X |
| 输入纯空格 | 仅一个按钮，点击后清空 |
| 点击清除 | 查询清空、按钮消失、未筛选列表和原折叠状态恢复 |
| Escape | 维持原有清空行为 |
| Tab 导航 | 自定义按钮可通过键盘到达并激活 |
| 默认及最窄侧栏宽度下输入超长文本 | 光标附近文字不被按钮遮挡，按钮可点击 |
| 深色、浅色主题及小、标准、大字号 | 仅一个按钮，清晰可见，不遮挡输入文字 |
| Cmd+F 查找栏 | 关闭与文本查找行为未受影响 |

移动端补充回归：输入框已有字号、最小高度和间距保持不变；非空时仅一个清除按钮，点击清空正常。未完成的真机检查必须明确标为未验，不得写成通过。

## 8. 完成判据

实现代码、相关自动化测试、前端构建、项目提交门禁和真实 macOS Desktop 验收全部通过，才能将项目管理条目标为已完成。移动端实际验收情况必须单独记录；如未验，不声称已验证移动端兼容性。

spec 编写完成不代表修复完成，本轮不更新为已修复、不合并实现、不打包或替换应用。

## 9. 风险与回滚

主要风险是选择器作用范围过大或右侧留白不足。通过专用类限定、最窄侧栏宽度及全部字号下的真实 WebView 验收控制。

回滚仅撤销专用类、右侧额外留白、对应 CSS 及新增回归约束；不涉及数据迁移或后端兼容。回滚意味着恢复旧行为，不能继续保留“已修复”的项目状态。

## 10. Spec 自检

- 根因证据、调查测试结果、拟实施内容及尚未完成的真机验收已区分。
- 范围仅限侧栏搜索框，无全局搜索框行为变更和无关重构。
- 非空定义、状态更新路径、样式作用域和不改变焦点策略均明确。
- 自动化测试不替代原生控件视觉验收；完成判据不以 spec 落盘代替修复。
- 两轮审查已核对代码、测试入口、Tailwind 留白、移动端复用、键盘前提和隔离验收；补齐 CSS 作用域回归的具体判据。
- 子代理审查因运行时 `invalid_state: a settled parent task (state: stalled) cannot spawn descendants` 未能启动；两轮核对由本会话完成，不宣称有独立审查结论。
- 无未决设计占位项。用户已授权审查无阻塞后编写实施计划，见 [实施计划](../plans/2026-11-05-sidebar-search-clear-impl.md)；用户已进一步授权将文档先合入 main，再正式投入看板，由看板调度实施；不在文档会话直接实施。
