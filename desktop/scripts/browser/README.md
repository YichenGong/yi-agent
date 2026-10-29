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
