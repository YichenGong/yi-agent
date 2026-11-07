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
