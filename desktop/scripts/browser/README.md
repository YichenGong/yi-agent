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
