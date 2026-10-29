# 会话页位置与选中会话的恢复 — 设计

日期：2026-10-25；链路复核：2026-10-29。
范围：桌面端 / 手机端共用的 React 客户端（`desktop/src`）。

## 1. 目标与边界

1. A→B→A 返回 A 上次离开时的阅读位置，而非最新消息。
2. 记录「视口顶部那条消息 + 消息内部已滚过的像素偏移」，不是跳到整块消息的起点；同一布局下恢复到原来的行。
3. 离开时贴底，回来继续贴底并跟随新内容。
4. 切别的 app 再回来，保持当前 session；保留前台重连，sidecar 崩溃仍能自愈。
5. 冷启动恢复最后选中的 session，存在性以服务端列表为准。
6. 重连或重复选择同一会话，不清右侧面板 / diff / 进程详情；换到不同会话时仍清除错配的数据。

非目标：其他页面的位置记忆；消息每个文本节点的字符级锚点；持久化阅读锚点到磁盘。冷启动只恢复会话身份，阅读位置保存在当前 App 实例内存中。布局宽度变化使正文重新换行时，像素偏移不保证仍指向同一个字符。

## 2. 已确认根因

### 2.1 原始选择逻辑

`ChatView` 曾在 `threadChanged` 时无条件 `scrollToBottom()`，没有按会话保存位置；`App` 握手曾只给手机端保留 `keptId`，桌面重连回落到列表第一条。冷启动也没有独立的「最后选中会话」存储。

### 2.2 真实浏览器几何与生命周期缺陷

2026-10-29 使用真实 `ChatView`、CSS、React DOM 提交和 Chrome 滚动事件复核，确认仅靠伪造 DOM 坐标的单测不够：

- **无盒锚点**：`data-item-id` 曾挂在 `display: contents` 节点，其 `getBoundingClientRect()` 没有消息真实坐标，采集会选到最后一条。A 离开时 `scrollTop=1800`，返回却到 `7684`（底部）。
- **归属串写**：旧会话的采集 rAF 可以在新 DOM 提交后才执行，闭包里的旧 id 不能证明 DOM 仍属于旧会话。
- **空/部分回放覆盖**：恢复完成前的采集会把旧锚点写成 `null`；锚点尚未回放到达时，跟随分支也可能抢先到底。
- **重挂载抢位置**：`firstRun` 强制到底会覆盖 layout effect 的有效恢复；目标会话已有的错误 / 本地用户尾消息不属于新事件。
- **最后一帧丢失**：用户滚动后立刻切换，采集 rAF 在清理时被取消，保存的仍是上一帧的位置。
- **恢复访问状态残留**：A→尚未恢复的 B→A 不能继续沿用上次 A 的 `restoredFor` / `userTookOver`。
- **程序化 scroll 回声**：0ms 定时器清除写入标记早于真实浏览器的 scroll steps，恢复/校正被误认为用户接管，后续增高校正失效。16ms 回声单测修复前停在 `1012`，期望 `1312`。

## 3. 位置模型

```ts
interface ViewAnchor {
  itemId: string; // 服务端协议 id；local-* 不参与锚定
  offset: number; // 消息顶边相对视口顶边，负数表示滚过消息的一部分
}
```

`undefined` 表示从未记录，`null` 表示离开时贴底，其他值表示阅读锚点。模型及纯函数在 `desktop/src/lib/viewAnchor.ts`。

采集和恢复统一坐标：

```text
消息内容坐标 = 消息 rect.top - 容器 rect.top + 容器 scrollTop
记录 offset = 消息内容坐标 - scrollTop
恢复 scrollTop = max(0, 当前消息内容坐标 - offset)
```

首条消息因容器 padding 出现在视口顶边以下时，仍可作为锚点，不能把「正在读最老消息」误记成贴底。

## 4. 保存链路

位置放在 `useSessionState.viewAnchors` 的 ref Map 中，`rememberView` / `anchorFor` / `forgetView` 按 thread id 操作，不驱动额外渲染。删会话时删除其记录。

### 4.1 可测量的消息盒

`ChatItem` 用 `flex shrink-0 flex-col` 包装每条消息并挂 `data-item-id`，不再使用 `contents`。外壳提供真实布局盒，内部气泡继续用 `self-start` / `self-end` 保持对齐，且可被 `ResizeObserver` 观察。消息 id 不作 CSS 选择器插值，扫描属性值以兼容任意协议 id。

### 4.2 滚动采集与离开前同步保存

- 滚动时按 rAF 合并采集。
- `App` 在选择不同会话、新会话回包后实际切换、进入看板之前，调用 `captureTranscriptRef.current?.()` 同步采集旧 DOM；此时再改变选中会话。
- `ChatView.captureViewRef` 注册同步采集函数，清理时仅清除本 effect 注册的句柄，取消待执行帧。
- 滚动容器带 `data-thread-id`；采集必须同时满足 DOM owner 与闭包 owner 一致、该 owner 已完成恢复、DOM 中有消息。
- 空 DOM / 尚待恢复的回放不报告位置，不抹掉旧锚点。
- 贴底判断复用 `useStickToBottom.isFollowing()` 的相对滚动意图，不能用内容增长后的绝对 gap 重算。

不能在旧 effect cleanup 中读取新提交的 DOM 来推断离开位置。

## 5. 恢复与跟随

### 5.1 每次访问的恢复状态

`threadId` 变化时，在 layout effect 中重置 `restoredFor`、`heldAnchor`、`userTookOver` 和程序化滚动标记。恢复状态属于本次访问，不能从上一次 A 留下来。

恢复 effect 依赖 `threadId`、`anchor`、`items.length`、`syncing`：

1. 无阅读锚点：采用默认贴底行为。
2. 有锚点：立即 `releaseFollow()`，防止回放期间抢到底。
3. 空 DOM、syncing 中还没出现目标消息，或目标已到但回放高度不足以容纳原 scrollTop：保持待恢复，后续内容到达再试。
4. 找到目标：在绘制前按统一坐标对位，记录浏览器夹取后的实际写入值，成功后才锁定本次恢复。
5. 完整内容中目标确实消失：回退到底部。

首次挂载、换会话以及待恢复期间，有阅读锚点则跟随 effect 不抢视口。新错误 / 新本地用户消息只有在同一会话的后续更新中才算显式到底事件。

| 场景 | 行为 |
| --- | --- |
| 首次进入，无记录 | 到底 |
| 回访，有非贴底记录 | 消息 + 内部偏移恢复 |
| 回访，贴底记录 | 到底并跟随 |
| 重挂载，有阅读锚点 | 恢复，不由 firstRun 覆盖 |
| 冷回放暂不含锚点 | 等待，不覆盖记录、不误判删除 |
| 完整历史已删除锚点 | 回退到底部 |
| 当前会话用户新发送 / 新错误 | 强制到底 |
| 切入会话已有历史错误 | 保持恢复位置 |

### 5.2 异步高度变化

恢复后保存 `heldAnchor`。图片加载、工具卡变化等改变上方消息高度时，`ResizeObserver` 调度 rAF 重新对位；无该 API 时以 100ms 总高度轮询退化。观察生命周期依赖内容条数与同步状态，冷回放恢复成功后也会建立观察，并覆盖新增消息。

用户主动滚动后设置 `userTookOver`，不再校正。程序化写入产生的 scroll 回声按实际位置匹配后消费，不能用 0ms 定时器假定事件已到达。下次进入会话重新恢复和校正，不继承上次用户接管状态。

## 6. 会话身份和附属面板

- 前台重连：握手使用两个平台共有的 `sess.currentIdRef.current`。仍在服务端列表里就保留，消失才回落。
- 冷启动：`selectedThread.ts` 读写 `yi-agent.selectedThread`，选择/新建时写，删当前会话时清空。存储失败不阻止 App；选中优先级是「当前 → 持久记住且仍存在 → 第一条」。
- 同一会话重复选择/重连仍进行内容对账，但不清其面板、diff 和进程详情。只有实际换会话才清错配内容。

## 7. 验证

```sh
cd desktop
npx vitest run src/lib/viewAnchor.test.ts src/lib/useStickToBottom.test.ts src/lib/selectedThread.test.ts src/components/ChatView.test.tsx src/App.test.tsx
npm run test:browser:session-view
npx tsc --noEmit
npm run build
```

真实浏览器回归脚本 `desktop/scripts/browser/session-view-check.mjs` 使用真实组件/CSS/事件，并在测试页关闭 Chrome 自带 scroll anchoring，避免浏览器自动兜底掩盖 WebKit 中的缺陷。Chrome 临时配置放系统临时目录，CDP 按 page 类型与 URL 精确连接，不能选扩展后台 target。

覆盖：A→B→A、多次往返、贴底、重挂载、分帧冷回放、锚点已到但尾部高度不足、A→尚未恢复的 B→A、历史错误、切换前同步保存最后一帧、A/B 独立位置、上方内容连续增高、用户滚动交还控制权。

反证：只把消息外壳回退为 `contents`，浏览器回归立即错选最后消息 `A-39` 而失败；恢复真实盒后通过。迟到回声单测把事件延迟到 16ms，能够捕捉 0ms 清标记缺陷。

Chrome 自动化不等于运行中的正式 `.app` 已更新，也不代替原生 WebKit 真机验收。浏览器测试依赖和参数见 `desktop/scripts/browser/README.md`。
