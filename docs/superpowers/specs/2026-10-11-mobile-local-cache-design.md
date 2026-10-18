# 手机端本地缓存设计（首屏不空、连上即刷）

> 目标：手机端（iOS WebView 客户端）冷启或断线重连时，界面不再长时间空白——
> 侧栏会话列表**首帧**即有，点进会话也能立刻看到上次的内容；连接建立后由服务端
> 权威数据就地覆盖并清除缓存痕迹。桌面端行为逐字节不变。
>
> 日期：2026-10-11

## 1. 问题

手机端是 app-server 的**远程客户端**：通过中继（`wss://`）连桌面进程。
`App.tsx` 的连接 effect（`desktop/src/App.tsx:1023`）在握手成功后依次发：

1. `initialize`
2. `ui/settings/read`
3. `workspace/list`
4. `thread/listAll` → `setGroups` / `setPinned` / `store.seed`
5. 选中第一条 → `thread/resume` 回放整段历史

**在 listAll 与 resume 返回之前，界面就是空的。** 手机走中继、往返慢，且
iOS 从后台切回前台会重建连接并重放整个握手（`App.tsx:1073` `onVisibilityChange`），
于是每次打开都是一段可感知的空窗。

现状盘点：

- 唯一的本地缓存是主题（`localStorage["app.theme"]`，`desktop/src/lib/theme.ts`
  + `index.html` 的首帧 inline script）。仓库既有契约：**权威数据在服务端，
  localStorage 只做首屏缓存**（`docs/superpowers/specs/2026-10-02-desktop-settings-theme-design.md` §149）。
- 没有任何会话列表 / 对话内容的本地缓存。
- `Session.upsertItems`（`desktop/src/lib/session.ts:128`）按 item `id` 去重、
  就地替换——服务端项到达即覆盖同 id 的旧项。这为"先显示缓存、再就地换新"
  提供了天然的收敛语义。
- `thread/resume` 会回放该会话**全部**历史（`docs/project-management/yi-agent-app-server.md`）；
  远程 running 会话走只读的 `thread/readItems` 补尾部（`App.tsx:642`）。

## 2. 目标与非目标

**目标**

- 手机端冷启 / 重连：侧栏列表**首帧可见**，无需等 `thread/listAll`。
- 点进会话：先显示上次缓存的内容，`thread/resume` 返回后就地替换为权威内容。
- 断网时明确标注"离线内容"，避免用户把缓存误当实时。
- 连接建立后一切以服务端为准，缓存痕迹（含幽灵项）被清除。
- 桌面端行为逐字节不变。

**非目标**

- 不做离线写入 / 发送队列重发（离线时发送挂起并提示，不排队）。
- 不做跨设备同步、不做服务器端改动。
- 不做 diff / 看板 / 子 agent 等其他面板的缓存。
- 桌面端不启用本缓存。

## 3. 已确认的决策

| # | 决策 | 取值 |
| --- | --- | --- |
| 1 | 范围 | 会话列表 + 对话内容 + 状态徽标（全部三类空窗） |
| 2 | 生效端 | 仅手机端（`isRemoteClient()` 门控），桌面端不变 |
| 3 | 存储介质 | 列表元数据 → localStorage（同步，首帧）；对话内容 → IndexedDB（异步） |
| 4 | 内容粒度 | 有界 LRU，只缓存最近打开过的若干会话 |
| 5 | 上界 | 最近 **10** 条；单条 **1MB**（超出丢头部保留尾部）；总量 **5MB** |
| 6 | 对账语义 | 缓存只做首屏占位，**服务端始终权威** |
| 7 | 离线提示 | 要标注"离线内容"；状态徽标降级；离线不显示未读点 |
| 8 | 离线输入 | 可打字（草稿按 thread 隔离照存），发送挂起提示，不自动重发 |
| 9 | 上次选中 | 冷启恢复 `selectedId`（在列表内时）；顺带修手机端"切回前台跳回首个会话" |

## 4. 架构

新增两个模块，`App.tsx` 只做接线。

### 4.1 `desktop/src/lib/sessionCache.ts`（列表层，同步）

- 后端：localStorage，键 `yi-agent.threadListCache`（与 `yi-agent.remote` /
  `yi-agent.sidebarWidth` 命名一致）。
- 存储经可注入的 `StorageLike`（沿用 `remoteConfig.ts` 惯例），默认
  `globalThis.localStorage`，不可用时退化为空操作。
- 纯函数：`serializeCache` / `parseCache` / `pickCacheable`，全部可脱离 DOM 测试。

格式：

```jsonc
{
  "version": 1,
  "savedAt": 1728700000000,
  "selectedId": "thread-…" | null,
  "pinned": [ /* ThreadSummary */ ],
  "groups": [ /* WorkspaceGroup */ ]
}
```

- 读取：解析失败 / `version` 不符 / 缺字段 → 返回 `null`（退化为现有空列表，不崩）。
- 只存 `ThreadSummary` / `WorkspaceGroup` 这类纯数据；**不存** `approval`、
  `unread` 等瞬时值（状态徽标靠 `store.seed` 播种，`statusSource="snapshot"`
  已保证不被回滚，见 `threadStore.ts:seed`）。
- 写入时机：`refreshThreads()` 成功、`handshake` 的 `thread/listAll` 成功后
  （即拿到权威列表时）节流写一次；`selectThread(id)` 时更新 `selectedId`。

### 4.2 `desktop/src/lib/contentStore.ts`（内容层，异步）

- 后端：IndexedDB，库 `yi-agent-cache`，store `threadContent`，key = `thread_id`。
- 对外经接口 `AsyncContentStore` 暴露（`read` / `write` / `delete` / `keys`）。
  真实实现是 IndexedDB；测试注入内存假实现（jsdom 无 IndexedDB，
  且不引入 `fake-indexeddb` 依赖）。

格式（每条）：

```jsonc
{
  "threadId": "thread-…",
  "savedAt": 1728700000000,
  "lastServerItemId": "item-…" | null,
  "items": [ /* 仅服务端 item */ ],
  "trimmed": false,
  "bytes": 12345
}
```

- **只存有服务端 id 的 item**（`userMessage` / `agentMessage` / `toolCall`）；
  丢弃 `local-*` 的乐观回声与 `notice`（命令输出，与上下文无关，重放会误导）。
- 单条裁剪 `applyTrim(items, MAX_BYTES)`：序列化后若超 1MB，**从头部**丢最旧的
  item 直到达标，置 `trimmed: true`（保留尾部 = 最近的对话）。
- 写入时机：会话内容变更后**防抖**写；`pagehide` / 切后台 / 切走会话时立即 flush。

### 4.3 数据流（手机端）

```
冷启 → readListCache() → 侧栏首帧有列表；store.seed(缓存 summaries) 播种 cwd/model/状态
     → 若有 selectedId 且在列表内 → setCurrentId(selectedId) → 异步读 IndexedDB
     → session.upsertItems(缓存 items) 渲染对话（复用既有按 id 去重/就地替换）
     → 连接建立 → handshake：
          · thread/listAll 覆盖侧栏（权威）
          · 当前会话若未 warm → selectThread → thread/resume 权威回放
          · resume 完成后做内容对账（§5）
写入：内容变更 → 防抖写 IndexedDB；pagehide / 切后台 / 切走会话 → flush；LRU 淘汰
```

**关键原则：缓存状态与 `warm` 集合严格分离。** 从缓存预填**不**把会话标记为
`warm`，否则连上后会跳过 `thread/resume`，服务端历史永远合不进来。

**手机专属门控：** 所有读写包在 `isRemoteClient()` 判断内；桌面端走原路径
（初始 `groups=[]`、每次 handshake 选第一条、不读缓存、不碰 IndexedDB）。

## 5. 服务端权威对账

**A. 列表层（已天然收敛）** — `thread/listAll` 回来后 `setGroups/setPinned`
整体覆盖；`store.seed` 的 `statusSource` 保护 live 状态不回滚。无需特殊处理。

**B. 内容层 · idle 会话** — `selectThread` 对冷会话照常 `thread/resume`，回放经
`apply` / `upsertItems` **按 item id 就地替换**，缓存项即被权威项覆盖。

**C. 内容层 · 关键缺口：服务端"没有的 item"必须被清掉。**
缓存里可能存在服务端已不存在的 item（桌面端 `thread/clear` / 或被裁剪）。收敛规则：

- 新增 `Session.dropStale(serverIds: Set<string>)`：遍历 `items`，保留
  (a) `local-*` 本地项、(b) `id ∈ serverIds` 的项，其余删除。
- resume 开始时收集 `replayIds = new Set<string>()`：`apply` 路径凡收到服务端
  item（携服务端 id）即 `replayIds.add(id)`。
- resume 响应返回后调用 `session.dropStale(replayIds)`，一次性清掉缓存里服务端
  没有的 item。
- 三段式：**先显示缓存 → 服务端回放就地覆盖 → 清掉幽灵项**。最终态等于服务端。

> **修正（2026-10-17）**：`compact` **不是**幽灵来源——它落盘时 `items` 为空、
> 历史 item 分毫未动（`read_log` 跨行拼接）。真正让 item 消失的只有
> `thread/clear`。且 `clear` 后回放集为空会让本节的 `replayed.size > 0` 守卫
> 跳过清理，故收敛判据已改为按世代（`content_epoch`）。见
> `2026-10-17-client-state-negotiation-design.md` §1.2 与 §6。

**D. 远程 running 会话（`thread/readItems` 补尾部）不做删项。**
`readItems` 只补尾部、不是全量回放，凭它删项会误删缓存里服务端仍存在但本次
未回放的 item。规则：**只有全量回放（resume 成功）才允许对账删项。**

## 6. 离线态 UI 与交互

连接状态：复用现有 `status` state（初值 `"connecting"`，握手成功 `"connected"`，
失败 `"error: …"`）。派生 `offline = status !== "connected"`，**不新增状态机**。

1. **"离线内容"提示条** — 当 `offline && 当前内容来自缓存` 时，聊天区顶部显示
   `离线内容 · 更新于 <相对时间>`。连上且当前会话权威刷新后自动消失。
   缓存读入时记 `cachedAt`（取自条目 `savedAt`）到 `Map<threadId, number>`；
   `connected` 时清空。相对时间格式化抽纯函数 `formatRelativeTime(ms, now)`。
2. **状态徽标过期标注** — 离线时侧栏 `running` / `awaiting_approval` 徽标降级
   （灰化 + `title="可能过期"`）；离线时**不显示未读点**（缓存未读会误导）。
   逻辑限定 `isMobile && offline`；桌面端不动。
3. **发送行为** — 离线时输入框照常可打字（草稿按 thread 隔离照存），但发送被
   挂起：点击发送若 `offline`，不做乐观发送，只提示"正在连接，请稍候"，草稿保留。
   **不做**自动排队重发。依赖连接的操作（上传、审批）同样离线禁用。
4. **恢复上次选中** — 冷启若缓存有 `selectedId` 且在缓存列表内 → 直接选中它。
   手机端 `handshake` 的"选中第一条"仅在**当前没有已选会话**时执行；
   `selectedId` 不在服务端新列表里（会话被删）→ 回落第一条。
   桌面端保留原行为（每次选第一条）。

   **实现注记（保持选中时对当前会话再走一次 `selectThread` 的副作用）：**
   判定不看"这是第几次握手"，只看"当前会话是否仍在服务端新列表里"——冷启从缓存
   恢复的、或用户正看着的，都保持；重连（新进程记忆为空、`warm` 与订阅窗口已在
   `exited` 清空）对它再走一次 `selectThread`：冷则 `thread/resume`、`running` 则
   `thread/readItems`（不打断在途回合）、`warm` 则只换视图并补 `thread/subscribe`。
   副作用：`selectThread` 会 `resetThreadDiff()`，故**重连时若 diff 面板正开着会被
   收起**——会话未变、这是既有行为，且 diff 面板在窄屏（手机端）本就不渲染，可接受。

## 7. 模块边界

| 单元 | 做什么 | 依赖 |
| --- | --- | --- |
| `sessionCache.ts` | 列表元数据的存取与淘汰选择；纯函数 | `StorageLike`（注入） |
| `contentStore.ts` | 会话内容的 IndexedDB 存取、裁剪、LRU；纯函数 + 薄适配层 | `AsyncContentStore`（注入） |
| `Session.dropStale` + replay id 收集 | 内容对账，删服务端没有的 item | 无（自有状态） |
| `App.tsx` | 接线：初始 state、握手后刷新、离线 UI、选中恢复 | 上述模块 + 现有 store/client |

## 8. 测试与验证

**纯函数单测（不碰 DOM / 存储）**

- `sessionCache.ts`：`serializeCache`/`parseCache` 往返；版本不符 / 坏 JSON /
  缺字段 → `null`；`pickCacheable` 的裁剪选择。
- `contentStore.ts`：`applyTrim` 从头部丢到达标、`trimmed` 标记；LRU 淘汰选择
  （给定条数 / 字节上界选该删哪些）；`bytes` 估算稳定。
- `formatRelativeTime(ms, now)`：秒 / 分 / 时 / 天边界；`now < ms` 防御。

**IndexedDB 适配层单测** — 内存假实现（不引入 `fake-indexeddb`）：put/get 往返、
delete、超上界触发淘汰、读缺失返回 `null`。真实 IndexedDB 由手机端手动验证覆盖。

**`Session.dropStale` 单测** — `session.test.ts` 构造"缓存含 A、B、C" +
"回放只含 A、C" → 断言 B 被删、A/C 保留、`local-*` 与 notice 保留、
`lastServerItemId` 不倒退。

**App 集成测试（e2e，mock RpcClient，jsdom）** — 在 `App.test.tsx` 范式上新增：

- 手机端 + localStorage 预置缓存列表 → 首帧断言侧栏已有条目（在 `thread/listAll`
  返回前）。
- 手机端 + 缓存 `selectedId` → 首帧断言选中该会话（而非第一条）。
- 连接建立后 `thread/listAll` 覆盖 → 断言缓存条目被权威列表替换。
- 离线时 `running` 徽标降级 / 无未读点；连上后恢复。
- 离线点击发送 → 断言只提示、不发 `turn/start`。
- **桌面端回归**：非 remote 时断言完全走原路径（初值 `[]`、handshake 选第一条、
  不读缓存、不发 IndexedDB 请求）——保证零改动。

**手动验证（真机 iOS WebView）** — 断网冷启看到上次列表 + 内容与"离线内容"
提示；恢复网络后就地刷新、提示消失；切后台再切回不跳回首个会话。

**验证命令**

```
cd desktop && npx vitest run \
  src/lib/sessionCache.test.ts \
  src/lib/contentStore.test.ts \
  src/lib/session.test.ts \
  src/App.test.tsx
cd desktop && npx vitest run   # 全量回归
```

无 Rust 改动，不跑 cargo。

## 9. 风险

- **幽灵内容**：靠 §5.C 的 `dropStale` 兜底；测试锁定。
- **缓存与 warm 混淆**：明确"预填不置 warm"，否则永不合流；测试锁定。
- **桌面端回归**：所有分支 gate 在 `isRemoteClient()`；专门回归测试。
- **localStorage 超限**：列表元数据小，仍有 `pickCacheable` + try/catch 兜底。
- **顺带修"切回前台跳回首个会话"**：属行为改进，单独测试锁定，仅手机端。
