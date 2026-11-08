# 会话阅读位置的真实浏览器回归

```sh
cd desktop
npm ci
npm run test:browser:session-view
```

要求 Node 22+（原生全局 WebSocket）及 Chrome。macOS 默认使用 `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`；Linux 默认 `/usr/bin/google-chrome`。可设置：

- `CHROME_BIN`：Chrome 可执行文件绝对路径。
- `SESSION_VIEW_PORT`：Vite 端口，默认 1438，需空闲。
- `SESSION_VIEW_DEBUG_PORT`：Chrome CDP 端口，默认 9348，需空闲。

不调用真实 LLM、sidecar 或生产会话数据。脚本自行启动 Vite 和独立 headless Chrome，退出时关闭其子进程并删除临时配置。临时配置放系统临时目录，避免触发项目的 Tailwind/Vite 文件监听。

测试加载真实 `ChatView` 与项目 CSS，用真实几何和浏览器 scroll/ResizeObserver 事件验证 A/B 独立恢复、分帧回放、重挂载、最后一帧保存、历史错误、异步增高和用户接管。测试页特意关闭 Chrome 自动 scroll anchoring，保证测到的是组件校正，而非浏览器兜底。

这不是正式 `.app` 的安装/发布，也不是原生 WebKit 真机验收；阅读锚点只存本实例内存，冷启动仅恢复最后选中的会话。

# 长会话的窗口几何与切换会话（真实浏览器回归）

```sh
cd desktop
npm run test:browser:transcript-window
```

`session-view` 那套 fixture 只有 40 条消息，全部落在首屏挂载范围内，因此两个 spacer
恒为 0、`scrollHeight` 只覆盖已挂载部分——**结构性地**测不到下面这两个缺陷：

1. **spacer 被 flex 压缩成 0 高度。** `ChatView` 的滚动容器是
   `flex min-w-0 flex-1 flex-col overflow-y-auto`。flex 子项默认 `flex-shrink: 1`，
   而 spacer 没有内容、`min-height: auto` 解析为 0，于是它被压成 0：真实浏览器实测
   `style.height = 53984px` 而 `getBoundingClientRect().height = 0`，容器
   `scrollHeight` 从 65833 掉到 11849。后果是未挂载区间完全不占位——滚动条比例失真，
   滚到该区间时视口里没有任何已挂载条目（**画面空白**），窗口翻页依赖的滚动几何也全错。
2. **转录窗口跨会话继承。** 窗口状态存在 hook 自己的 state 里、不与 `threadId` 绑定。
   在 A 里上翻到历史中间再切到等长的 B，B 直接落在 A 的中间区间（实测 260 条、首条是
   B 的中段），用户看不到最新内容，要切走再切回来才恢复。

fixture 用两条 **600 条**等长会话（id 前缀 A/B）+ 820px 视口，断言四件事：spacer 的
渲染高度等于样式高度、`scrollHeight` 覆盖未挂载区间、滚到中部能翻页且视口里有条目、
切会话后回到新会话的尾部首屏 60 条。**修复前实测红**：`styled 110160px, rendered 0px`
（spacer）与 `got 260 mounted (inherited window 260)`（跨会话）。jsdom 没有布局引擎，
这些断言在那里恒为 0、只会空转，所以必须真实浏览器。

- `TRANSCRIPT_WINDOW_PORT`：Vite 端口，默认 1441，需空闲。
- `TRANSCRIPT_WINDOW_DEBUG_PORT`：Chrome CDP 端口，默认 9351，需空闲。
- `CHROME_BIN`：Chrome 可执行文件绝对路径。

不调用真实 LLM、sidecar 或生产会话数据，退出时关闭子进程并删除临时 Chrome 配置。

# 流式正文的实时可见性（真实浏览器回归）

```sh
cd desktop
npm run test:browser:stream-live
```

`ChatView` 里「跟随内容增长」的 effect 必须由**可见文本变化**驱动，而不能由
「尾部项是不是 agentMessage」推导。服务端会在仍在流式的回答之后插入 CoT 块或
工具卡（`translate.rs` 的 `ThinkingBlock` / `ToolCall` 两条臂），那一刻回答被顶
到 `length - 2`；若跟随信号仍从尾部文本推导，它恒为空、effect 不再重跑，视口
就冻在原地——新内容全落在可视区下方，界面看起来像「回复没出现」，而切走再切回
（重挂载强制贴底）又能看到。jsdom 没有布局引擎，测不出这种「内容在 DOM 里但
不在视口内」的差别，所以这条判据只能在真实浏览器里验收。

本脚本跑两个场景：回答是尾部项（基线），以及回答已被 CoT 块 + 工具卡顶到非尾部
（回归）。两者都必须 `atBottom`，并且尾部文本必须真的进入 DOM。

- `STREAM_LIVE_PORT`：Vite 端口，默认 1439。
- `STREAM_LIVE_DEBUG_PORT`：Chrome CDP 端口，默认 9349。
- `CHROME_BIN`：Chrome 可执行文件绝对路径。

不调用真实 LLM、sidecar 或生产会话数据，退出时关闭子进程并删除临时 Chrome 配置。

# 子 agent 轨迹面板（真实浏览器验收）

```sh
cd desktop
npm run test:browser:subagent-trace
```

`SubagentTrace` 的几何不能由 jsdom 证明：jsdom 没有布局引擎，`scrollHeight`、
`clientWidth`、`ResizeObserver` 全是不存在的，横向溢出和「正文还剩多少像素」在
那里永远是 0。本脚本用与上面两个脚本相同的 Node 原生 WebSocket + CDP 方式，
在一张真实页面上渲染 **真实的** `ThreadDetailPanel → SubagentTrace →
TraceTimeline → TraceEvent → TraceComposer` 与项目自己的 `index.css`，然后测：

- 主题 2 × 栏宽 3（260/360/720）× 字号 3（小/标准/大）= **18 种组合**，全部在
  最小窗口 900×600 下：零整栏横向溢出、主滚动区 ≥120px、消息框与动作可达；
- 长中文、无空格长路径、Markdown 表格/代码、错误结果、多个子任务、真实空元数据；
- 渐进窗口：1000 块历史首屏只挂 200、「载入更早」每批 +200、追加不收窄已挂载范围；
- 阅读锚点：真实滚轮暂停后，追加同块文本 / 展开收起摘要 / 图片加载后变高，
  锚点偏移误差 ≤2px；滚回底部恢复跟随；
- 键盘：确认区打开时焦点落在「放弃」，Tab 离开时仍在面板内；
- reduced-motion、`role="log"` 不是 live region、`aria-expanded`/`aria-controls` 完整；
- 深/浅主题下**所有**渲染出来的文字都满足 WCAG 对比度下限（小字 4.5:1）。

环境变量：

- `SUBAGENT_TRACE_PORT`：Vite 端口，默认 1449，需空闲。
- `SUBAGENT_TRACE_DEBUG_PORT`：Chrome CDP 端口，默认 9359，需空闲。
- `CHROME_BIN`：Chrome 可执行文件绝对路径。
- `SUBAGENT_TRACE_REPORT`：报告 JSON 路径，默认 `.artifacts/desktop-subagent-trace/report.json`（仓库根，已被 gitignore）。

不调用真实 LLM、sidecar 或生产会话数据；脚本自行启动 Vite 与独立 profile 的
headless Chrome，退出时先 TERM 后超时 KILL，并删除临时 profile。缺 Chrome 时
脚本会明确失败，不会退化成 jsdom 的"通过"。

这仍然**不是**原生 WKWebView 真机验收：Chrome 的排版引擎与 WebKit 不同，IME 与
真实的触控滚动只能人工验证。README 与验收文档必须把两者分开记录。
