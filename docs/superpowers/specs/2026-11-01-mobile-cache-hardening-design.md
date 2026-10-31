# 手机端本地缓存的加固与隔离

> 目标：手机端首屏缓存（`sessionCache.ts` + `contentStore.ts`）目前有两处会让
> 它**在真实使用中失效或骗人**的缺陷——空的记录会把真正的内容挤出缓存（§1.1），
> 以及换一台电脑配对后仍会显示上一台电脑的会话与对话（§1.2）。本文给出这两项的
> 修法，并把旧设计文档里与实现不符的三处陈述订正，同时给列表快照加一个上界。
> 桌面端行为逐字节不变。
>
> 日期：2026-11-01
> 相关：`2026-10-11-mobile-local-cache-design.md`（本地缓存；本文**修正**其
> 旧 §4.1 的符号名与写入时机、旧 §8 的测试清单、旧 §9 的超限兜底，
> 见 §7 修正表）、
> `2026-10-17-client-state-negotiation-design.md`（`content_epoch` 骨架；
> 本文依赖其 §4.3 的世代判据，不重复设计）

## 1. 问题

### 1.1 空记录撑爆 LRU，把真正的内容挤出去

三处叠加，缺一不可：

1. `threadStore.seed()`（`desktop/src/lib/threadStore.ts:140`）为列表里
   **每一个**会话 `view()` 建视图，于是 `store.ids()` 等于整个侧栏会话数。
2. `flushAll`（`desktop/src/App.tsx:637-644`）在页面隐藏 / 卸载时遍历
   `store.ids()` 全部写盘；`persistContent`
   （`desktop/src/lib/contentStore.ts:205-223`）**不跳过空 items**，且每次写都
   刷新 `savedAt: Date.now()`（`:216`），随后 `await evict(store)`（`:223`）。
3. `selectEvictions`（`contentStore.ts:51-65`）按 `savedAt` 升序淘汰，
   `CONTENT_LIMITS.maxCount = 10`。

结果是：切一次后台，几十个**从未打开过**的会话被写成「空记录 + 最新时间戳」，
把有内容的会话按 LRU 淘汰。侧栏会话数超过 10 时，这个缓存对它的目标场景
（下次打开先看到上次的内容）基本失效。

**实测**（临时 worktree 中用真实 `persistContent` 跑，非推理；该 worktree 已清理，
两个场景由 §5 `App.test.tsx` 的 (a) 用例永久锁定，故论据可复验）：

- 1 个有内容会话 + 12 个空会话 → 有内容的条目被删。
- 侧栏 14 个会话、用户只读过其中 2 个 → 存活 keys 为
  `['never2', 'never3', 'never4', …(7)]`，**`read1` / `read2` 全被挤掉**。

### 1.2 缓存键不分服务端，换配对后显示另一台电脑的数据

三个存储键都是全局固定名，没有任何服务端标识：

- `THREAD_LIST_CACHE_KEY = "yi-agent.threadListCache"`
  （`desktop/src/lib/sessionCache.ts:13`）
- IndexedDB 库名 `DB_NAME = "yi-agent-cache"`（`contentStore.ts:120`）
- `SELECTED_THREAD_KEY = "yi-agent.selectedThread"`
  （`desktop/src/lib/selectedThread.ts:13`）

而 `clearRemoteConfig()`（`desktop/src/lib/remoteConfig.ts:74`）在整个
`desktop/src` 下**没有任何调用点**；配对成功只调 `saveRemoteConfig`
（`desktop/src/components/PairingScreen.tsx:149`），不清任何缓存。

于是手机从 A 电脑改连 B 电脑（或对同一台重新配对）后，首帧会画出 A 的会话
列表，点进去显示 A 的对话内容。虽然服务端列表回来就会覆盖，但**显示的是另一台
机器的数据**——这不只是「过期」，是串了别人的内容，也构成隐私泄露面：
缓存里存的是完整对话原文，而没有任何「清除缓存」入口。

### 1.3 旧设计文档与实现漂移

`2026-10-11-mobile-local-cache-design.md` 作为权威已不可信（下表「位置」一列
**全部指旧文档的章节号**，不是本文）：

| 旧文档位置 | 文档说 | 实际 |
| --- | --- | --- |
| 旧 §4.1 | 导出 `serializeCache` / `parseCache` / `pickCacheable` | 只有 `parseListCache` / `readListCache` / `writeListCache`（`sessionCache.ts:30,73,83`）；前三个符号**不存在** |
| 旧 §4.1 | 写入时机：`refreshThreads()` 成功、握手 `thread/listAll` 成功后；`selectThread(id)` 时更新 `selectedId` | 写点实为两处：握手（`App.tsx:1676`）与 `refreshThreads` 的 `thread/listAll` 成功后（`useSessionState.ts:127`）。快照里的 `selectedId` 确实存在（`sessionCache.ts:93`，写入时取 `store.currentId`），但**不存在**「`selectThread` 时更新」——它只在下次列表写入时被动刷新 |
| 旧 §8 | 测试 `serializeCache`/`parseCache` 往返、`pickCacheable` 裁剪 | 不存在对应测试与函数 |
| 旧 §9 | 「仍有 `pickCacheable` + try/catch 兜底」 | 只有 try/catch 静默吞掉（`sessionCache.ts:99`） |

### 1.4 列表快照没有上界

`writeListCache`（`sessionCache.ts:83`）直接序列化整个 `groups`，无条数 /
字节上限，超配额时被 `catch` 静默吞掉（缓存整个丢失，退化为冷启动）。

尺度实测（`node` 跑真实 `JSON.stringify`，字段取 `ThreadSummary` 的全部必填项
加 `permission_mode`/`status`/`pinned`/`project_root`，即最坏形状）。载荷大小
**强依赖 `cwd` 与 `title` 的长度**，故给区间而不是单值：

| 会话数 | 短路径 + 短标题 | 真实长路径 + 长标题 |
| --- | --- | --- |
| 50 | 12 KB | 18 KB |
| 200 | 50 KB | 70 KB |
| 500 | 124 KB | 176 KB |
| 1000 | 248 KB | 352 KB |
| 2000 | 497 KB | 704 KB |

**结论修正**：早期评审中「几千会话会逼近 localStorage 5MB」的说法不成立——
按最坏形状也要约 1.45 万条才撞 5MB（短形状约 2.06 万条）。故本条定性为**防御性有界**（低优先级），
不是必要修复。真实风险不是配额，而是「无界」这件事本身：载荷大小取决于服务端
返回多少会话，客户端不应假设它有限。

（1000 条上限对应的最坏载荷约 352 KB，相对 Safari 移动端 localStorage 的
5 MB 量级有 14 倍余量，选 1000 不需要再按字节收紧。）

## 2. 目标与非目标

**目标**

1. 缓存不再自我淘汰：空的 / 从未打开过的会话不占 LRU 槽位；内容没变的会话也不
   重复写盘（否则每次切后台都把 `savedAt` 抬到当下，LRU 顺序被抹平成迭代顺序）。
2. 换配对 / 换电脑后不显示另一台机器的数据。
3. 旧设计文档的三处错误陈述被订正，后来者不会被不存在的符号误导。
4. 列表快照最坏情况有确定上界。

**非目标**（明确不做，各有理由）

- **`savedAt` 一字段两语义拆分**（`lastAccessedAt` / `contentUpdatedAt`）：
  它是「淘汰排序键（最近写入）」与「『更新于』展示值」的合体，拆分需同时提升
  `THREAD_LIST_CACHE_VERSION` 与 `DB_VERSION`，属独立一期。
- **「清除缓存」UI 入口**：新增界面元素，应单独评审。本期只用 §4.2 的
  scope 失配自动清理覆盖「换机」这一主要场景。
- **IndexedDB `open` 失败被永久缓存**（`contentStore.ts:141` 的
  `dbPromise ??= open()`）：一次瞬时失败后本会话再无缓存，需重试策略，独立问题。
- **多 WebView 实例协调**（`onblocked` / `onversionchange`）：本期不引入。
- **不做任何存储结构版本号提升**：本期所有改动都是「加一个键 + 改判定逻辑」，
  已有 v1 载荷的格式不变。
- **真正的「访问序 LRU」**：`savedAt` 是「最近写入」时间，读取路径不更新它
  （`contentStore.ts:47-49` 的注释自己写的是 "refreshed on every write"，严格说是
  写序而非访问序）。本期不改，见上面 `savedAt` 拆分那条。
- **`selectedId` 预填判据只看 `groups`、不看 `pinned`**（`App.tsx:620`）：既有行为，
  与本期无关，登记备查（详见 §4.3 末尾）。

## 3. 已确认的决策

| # | 决策 | 选择 | 理由 |
| --- | --- | --- | --- |
| D1 | 本期范围 | 做 §1.1 + §1.2 + §1.3 + §1.4 | 同一次触碰代码能一并落地，不引入 schema 变更 |
| D2 | 空记录的修法 | `persistContent` 默认不写空内容 + `flushAll` 只刷「脏集 ∪ 指纹变化的会话」 | 两处各自独立有效；且「没变不写」避免反复刷新 `savedAt` 抹平 LRU 顺序 |
| D3 | 跨服务端隔离 | 启动期 scope 标记校验，url 失配即清 | 不动 DB 名与键名，无迁移、无孤儿库；配对成功时也重查一次，本次会话内即生效 |
| D4 | 文档漂移 | 新 spec 列修正表 + 回头订正旧文档 | 与 `2026-10-17` spec 的既有先例一致 |
| D5 | 容量上界 | 条数上限 1000 + 截断标记 | 确定性、可断言，无重试分支 |

## 4. 架构

### 4.1 空记录不再落盘（`contentStore.ts` + `App.tsx`）

给 `persistContent` 增加一个显式选项，把「没内容」的处置权交给调用点：

```ts
export interface PersistOptions {
  /**
   * 该会话的内容已与服务端对账完成（全量 resume 成功）。
   * 为 true 时，items 为空 = 服务端确认此会话没有内容 ⇒ 删除缓存条目；
   * 为 false（默认）时，items 为空 = 我们还没有内容可写 ⇒ 不写也不删。
   */
  authoritative?: boolean;
}
```

规则表（`serverItems` = 过滤掉 `notice` 与 `local-` 前缀后的 items）：

| `serverItems.length` | `authoritative` | 动作 |
| --- | --- | --- |
| > 0 | 任意 | 写入 + `evict`（与现在相同） |
| 0 | `false`（默认） | **直接返回**，不读不写不删 |
| 0 | `true` | `store.delete(threadId)` 后返回（不 evict，条目数只减不增） |

`persistContent` 的完整新实现（相对现有 `contentStore.ts:205-223` 的增量已标注）：

```ts
export async function persistContent(
  store: AsyncContentStore,
  threadId: string,
  session: PersistSource,
  options: PersistOptions = {},          // 新增
): Promise<PersistOutcome> {              // 原为 Promise<void>
  const serverItems = session.items.filter(
    (i): i is Item => i.type !== "notice" && !i.id.startsWith("local-"),
  );

  // 新增：空内容守卫。这是本期的核心，位置必须在 applyTrim 之前——
  // 对空数组做 trim 只会得到空数组，白算一次字节估算。
  if (serverItems.length === 0) {
    if (!options.authoritative) return { action: "skipped" };
    await store.delete(threadId);
    return { action: "deleted" };
  }

  const { items, trimmed } = applyTrim(serverItems, MAX_ENTRY_BYTES);
  const entry: ContentEntry = {
    threadId,
    savedAt: Date.now(),
    lastServerItemId: session.lastServerItemId,
    items,
    trimmed,
    bytes: estimateItemsBytes(items),
    contentEpoch: session.contentEpoch,
  };
  await store.write(entry);
  await evict(store);
  // 指纹由**写入的那个 entry** 算出，与冷启播种时的
  // contentFingerprintOf(entry.items, entry.lastServerItemId, entry.contentEpoch)
  // 逐字一致——这是「两侧同源」的保证，不能另算一遍 session.items。
  return {
    action: "written",
    fingerprint: contentFingerprintOf(entry.items, entry.lastServerItemId, entry.contentEpoch),
  };
}
```

关键点：指纹取自 `entry`（已 trim 的写入体），而不是 `session.items`。这样
「写入时记下的指纹」与「冷启时从盘上条目反算的指纹」是**同一个函数作用于同一份
数据**，天然相等；否则超 1MB 被 trim 的会话会永远判「变了」，每次切后台都重写。

为什么默认「不写也不删」而不是「删」：写空记录会污染 LRU（§1.1 的本体）；
而删掉一条可能完好的旧缓存同样有害——「预填了缓存、还没等到服务端回放就切走」
时 `items` 也是空（尚未注入），此时删除等于把好数据误删。删除权限只给
**确认过服务端状态**的调用点。

一个必须写清的边界：`authoritative: true` 时判据用的是**过滤后的
`serverItems.length`**，不是 `session.items.length`。用户刚发出一条消息、只有
本地回显（`local-*`）而服务端尚未回帧时，`session.items` 非空但 `serverItems`
为空——此时**不得**删条目（服务端还没有任何可对账的内容）。过滤规则与
`persistContent` 现有的 `serverItems` 过滤完全一致，不引入第二套判据。

唯一的 `authoritative: true` 调用点在 `App.tsx` 的 `selectThread` 全量 resume
成功分支（现有 `void flushContent(id)` 处，`App.tsx:913`），因为那里刚收到完整
回放并有 `dropStale` 结果可依。

`flushContent` 的**全部四处**调用点（逐一列出，避免实现时漏改）：

| 位置 | 语义 | `authoritative` |
| --- | --- | --- |
| `App.tsx:344` | `schedulePersist` 的防抖回调 | 默认 `false` |
| `App.tsx:643` | `flushAll`（页面隐藏/卸载） | 默认 `false` |
| `App.tsx:688` | **切走上一条会话**时立即落盘 | 默认 `false` |
| `App.tsx:913` | `selectThread` 全量 resume 成功后 | **`true`**（`{ authoritative: true }`） |

`:688` 容易被漏掉——它是「切走即落盘」的实际实现，不在 `flushAll` 里。它保持
默认值是对的（切换会话时另一条会话并未完成对账），但改动清单必须包含它。

`:913` 置 `authoritative: true` 的前提，是现码已有的那段守卫
（`App.tsx:904`：`if (replayed !== null && replayed.size > 0) replaySession.dropStale(replayed)`）。
**只有在真的收到过回放帧并做过 `dropStale` 时才权威**：`replayed` 为空意味着
「这趟 resume 压根没投递帧」，此时 `items` 仍可能是刚预填的缓存内容，
`session.items` 被清空并不代表服务端确认无内容。实现时 `authoritative` 必须与
这个守卫**同一个条件**，不能无条件传 `true`——否则会重演注释里已经写过的
「把刚预填的缓存整段抹掉」那个 bug，只是这次抹的是盘上的条目。

具体写法：把守卫的判定提成一个局部常量，两处共用，避免「改了守卫忘了改
`authoritative`」：

```ts
const authoritative = replayed !== null && replayed.size > 0;
if (authoritative) replaySession.dropStale(replayed);
// ……中间是既有的 warm/syncState/cachedAt 处理……
void flushContent(id, { authoritative });
```

注意 `:913` 的 `flushContent(id)` 现在位于守卫**之后**、无条件执行；加参数时
不能顺手把它挪进 `if (authoritative)` 里——`authoritative: false` 时仍然要落盘
（那是预填内容，必须缓存），只是不授权删除。

**与 `thread/clear` 的关系**：服务端清空会话由 `content_epoch` 世代判据处理
（`Session.adoptEpoch` 清空 items 并置 `epochInvalidated`，
`desktop/src/lib/session.ts:209`；依赖 `2026-10-17-client-state-negotiation-design.md`
§4.3，本文不重复设计）。世代变化后 `items` 为空，随后全量 resume 成功即走
`authoritative: true` 删条目——两条路径不冲突：世代负责「不再显示旧内容」，
本规则负责「不要把空态写成新记录」。

`flushAll` 同时收窄刷新范围，从「整个侧栏」改为「确实需要重写的会话」。
先补两个共用件——**指纹必须在归一化之后计算**，这是本节最容易写错的一处：

```ts
/** `persistContent` 到底动了盘上什么。调用方靠它决定 persistedFp 该 set 还是 delete。 */
export type PersistOutcome =
  | { action: "written"; fingerprint: string }
  | { action: "deleted" }
  | { action: "skipped" };   // 空内容且非 authoritative：没读没写没删

/**
 * 归一化：与 persistContent 内部**完全同一套**（先剔 notice / `local-*`，
 * 再按 MAX_ENTRY_BYTES 从头部裁剪）。指纹必须基于归一化结果，不能基于
 * `session.items`——否则含 notice 或有本地回显的会话，两边的条目数永远不等，
 * 指纹永远判「变了」，等于没有优化。
 */
export function normalizeForCache(session: PersistSource): { items: Item[]; trimmed: boolean } {
  const serverItems = session.items.filter(
    (i): i is Item => i.type !== "notice" && !i.id.startsWith("local-"),
  );
  return applyTrim(serverItems, MAX_ENTRY_BYTES);
}

/** 指纹：盘上条目的身份。入参是**归一化后**的条目列表。 */
export function contentFingerprintOf(
  items: Item[],
  lastServerItemId: string | null,
  contentEpoch: number | null | undefined,
): string {
  return `${items.length}:${lastServerItemId ?? ""}:${contentEpoch ?? ""}`;
}

const ids = new Set([
  ...persistTimers.current.keys(),                                  // 脏但未落盘
  ...store.ids().filter((id) => {
    const sess = store.peek(id)?.session;
    if (!sess) return false;                                        // 无会话，跳过
    const { items } = normalizeForCache(sess);
    const fp = contentFingerprintOf(items, sess.lastServerItemId, sess.contentEpoch);
    return fp !== persistedFp.current.get(id);
  }),
]);
```

`normalizeForCache` 里有 `estimateItemsBytes`（`JSON === stringify` + UTF-8 编码），
对大会话不是零成本。但它**只在 `flushAll`（页面隐藏/卸载）时跑一次**，且只对
`store.ids()` 里的会话做一遍；手机端侧栏几十条、总内容受 `CONTENT_LIMITS` 约束，
量级是几百 KB 的字符串化，可接受。若日后 profile 显示这步可见，用
`structuralVersion` 做记忆化，不要改成「只比长度」的近似——那会退回本节开头
说的不一致问题。

**为什么不能只刷 `persistTimers`**：内容进入会话有三条路径，只有一条会注册防抖。
`schedulePersist` 只在通知分发处被调用（`App.tsx:1533`），而
`thread/readItems` 的两个分支（`App.tsx:817` 静默补齐、`:865` 运行中会话）
直接 `upsertItems` **不注册防抖**，`prefillFromCache`（`:276`）同理。
若 `flushAll` 只看 `persistTimers`，这些会话在切后台时会丢掉首次落盘机会
（尤其「只读补齐过的空闲会话」：它不会有后续通知把内容再送进防抖）。

**为什么也不能用「有没有内容」当判据**（这是本 spec 前一版的设计，已废弃）：
它不区分「内容变了」与「内容一直在」。`persistContent` 每次写都刷新
`savedAt`（`contentStore.ts:216`），而 `savedAt` 正是 `selectEvictions` 的排序键
（`:55`）。于是每次切后台都会把**所有**有内容的会话重写一遍、时间戳一起抬到
"现在"，写序被抹平成「迭代顺序」。当有内容的会话多于 `CONTENT_LIMITS.maxCount`（10）
时，被淘汰的是哪 10 条取决于 `persistTimers` / `store.ids()` 的遍历顺序，而不是
写入先后——§1.1 以另一种形态复现（间歇性丢缓存，且不稳定复现）。

**指纹判据解决什么、不解决什么（必须如实说明，不要过度声称）**：

- **解决**：切后台时不再重写「内容没变」的会话。典型受益者是**冷启后首次切后台**
  与**长期挂着的空闲会话**——它们会被 `persistTimers` ∪ 指纹过滤挡掉，不再无谓
  刷新 `savedAt`。
- **不解决**：脏集分支（`persistTimers` 里的 id）**无条件**加入待刷集合，不经过
  指纹。而 `item/delta` 恰恰会进脏集（`App.tsx:1533` 的 `schedulePersist`）。
  所以「多会话同时有后台流式输出」时，这些会话仍会被重写、`savedAt` 仍会被抬到
  当下，`CONTENT_LIMITS.maxCount` 的淘汰仍可能落到非最近写入的那几条。

为什么不在脏集上也加指纹：脏集的语义就是「内容有变化、尚未落盘」，对它做指纹
过滤等于放弃「流式文本已落盘」这一保证；而指纹（见下文）对同长度文本替换本就
不敏感，用它守脏集反而会静默漏写。这是**有意的取舍**：宁可多写几次，不可漏写。
真要消除这半个问题，需要把 `savedAt` 拆成 `lastAccessedAt` / `contentUpdatedAt`
两字段（§2 非目标），那才是彻底修法。

**为什么不用 `Session.structuralVersion`**：它在「文本增量追加到末项」时**不**自增
（`session.ts:638` 的注释；注意 `:621` 处「非末项」的 delta 仍会自增），所以它既不是
「永远不变」也不是「每变必增」——作为「是否需要重渲染」的判据是对的，作为
「内容是否已落盘」的判据会漏掉流式文本。`item/delta` 虽然会走 `schedulePersist`
（因此在脏集里），但把两种语义混在一个字段上会让后来者难以判断谁是对的。
指纹用「长度 + 游标 + 世代」三个既有的、与持久化直接相关的量，语义单一。

指纹的已知盲区：`upsertItems` 用同 id 就地替换、且返回集合不推进游标时，长度与
游标都不变 → 判为「没变」而不写。这种情形下被替换的内容是**同一批 id 的服务端
权威值**（回放/补齐的幂等重叠），与已落盘内容等价，不写是无害的。这个盲区要在
实现注释里写明，避免后人以为指纹是「内容哈希」。

**删缓存的四条路径**（下表）——清理必须**集中**，不要散落：

| 路径 | 位置 | 是否经 `deleteCachedContent` |
| --- | --- | --- |
| 删除会话 | `App.tsx:1227` | 是 |
| `thread/clear` | `App.tsx:1859` | 是 |
| `listAll` 报 `drop`（服务端已无此会话） | `App.tsx:748` | **否**（直接 `getContentStore().delete(id)`） |
| `authoritative` 空内容 | `App.tsx:913` → `persistContent` 内部删除 | 否 |

集中在哪里：**`persistedFp` 的更新只发生在 `flushContent` 内**，直接照着
`PersistOutcome` 分派（见上面代码）：`written` → `set(fingerprint)`、
`deleted` → `delete(id)`、`skipped` → 不动。判定权全部收在 `persistContent` 里，
`flushContent` 不再自己重新判断「`serverItems` 是否为空」——那会把规则表抄第二遍，
两处迟早漂移。

此外只有两处必须显式 `delete(id)`，因为它们**不经过 `flushContent`**：
`deleteCachedContent`（覆盖前两行）与 `App.tsx:748`（`drop` 分支）。

为什么不能只依赖「指纹偏旧只会多写一次」这个安全方向：`thread/clear` 之后
`session.reset()` 会把 `items` 清空、`lastServerItemId` 置 null（`session.ts:271-283`），
指纹随之变化。若此时 `persistedFp` 没有清，既有值仍对应「有内容」的旧指纹，两者
不等 → 会在下一次 `flushAll` 触发一次写。这次写**不会**写出旧内容（items 已空，
`persistContent` 的非 `authoritative` 路径会直接返回、不写不删），所以它是无害的
空转；真正的问题是**语义混乱**——`persistedFp` 描述的是「盘上是什么」，而盘上此时
已被删空，两者不一致会让后续每一次判断都多绕一次。清理它是为了让这个内存缓存的
含义始终是「盘上条目的指纹」，而不是靠「碰巧无害」维持。

`:748` 单独列出的理由：`store.drop(id)` 之后 `store.peek(id)` 为 `undefined`，
`flushContent` 会提前返回，因此**这条路径不会自己清理**；漏了它，该 id 的指纹会
一直留在 Map 里（内存泄漏量极小，但语义错）。

所有调用点（含 `flushAll` 与 `:913`）因此自动正确，不必各自记得。

**冷启时 `persistedFp` 为空，必须用盘上内容播种，否则首次切后台会全量重写一次**：
冷启后 `persistedFp` 是空 Map，而冷启的 `keys()` 扫描（`App.tsx:305-318`）已经把
盘上条目读进 `cacheMetaRef`。应在同一个循环里顺带
`persistedFp.set(id, fpFromEntry(entry))`——即用**盘上条目**反算指纹
（`contentFingerprintOf(entry.items, entry.lastServerItemId, entry.contentEpoch)`；
`entry.items` 已经是归一化后的写入结果，与写入时记下的指纹天然同源）。不留空的话，冷启后
第一次 `flushAll` 会把每个有缓存的会话都重写一遍、`savedAt` 全部抬到当下，又一次
抹平 LRU（§1.1 的同一机理）。这与「`cacheMetaRef` 由冷启扫描填充」是同一件事的
两个用途，放在一起做最自然。

**先改造 `flushContent` 的返回语义（否则下面的保护不成立）**：它现在是
`persistContent(...).catch(() => {})`（`App.tsx:353-355`），**catch 之后 Promise 恒
resolve**。也就是说「成功后」这个条件在当前实现下永远为真——写失败也会被当作
成功。必须先把 `flushContent` 改成返回真实的成败：

```ts
/**
 * 返回「动了盘上什么」，而不是一个 boolean。boolean 不够用：`written` 与
 * `deleted` 都算「没失败」，但 persistedFp 要做相反的操作（set / delete）。
 * 失败（抛错）或前置条件不满足 → 返回 null，表示**什么都没记**，下次自然重试。
 */
const flushContent = async (
  id: string,
  options?: PersistOptions,
): Promise<PersistOutcome | null> => {
  if (!isRemoteClient()) return null;       // 桌面端：本就没有缓存
  const v = store.peek(id);
  if (!v) return null;                      // 会话已不存在（如刚被 drop）
  try {
    const outcome = await persistContent(getContentStore(), id, v.session, options);
    // 只有 persistContent 成功返回才更新「盘上是什么」；顺序不可颠倒。
    if (outcome.action === "deleted") persistedFp.current.delete(id);
    else if (outcome.action === "written") persistedFp.current.set(id, outcome.fingerprint);
    // action === "skipped"：盘上没动过，persistedFp 保持原值。
    return outcome;
  } catch {
    return null;                            // 写失败：不抛给调用方，也不记指纹
  }
};
```

为什么不是返回 `boolean`：`true` 只能表达「没失败」，而「写了」与「删了」在
`persistedFp` 上要做相反的操作，`false` 也无法区分「写失败（该重试）」与
「空内容空转（不必重试）」。`PersistOutcome` 是这三个状态的唯一载体。

调用点：不关心结果的地方保持 `void flushContent(id)`；**不要**再包一层 `.catch`
吞错——吞掉之后「失败后重试」这条保护永远不会触发。

**`persistedFp` 只能在写成功之后更新，绝不能在写之前**：若先记指纹再写，
一次 IndexedDB 失败（配额、被系统回收、open 失败）就会把该会话永久标记为「已落盘」，
此后任何 `flushAll` 都判「没变」而不再重试——缓存静默永久丢失，且**没有任何错误
可见**。所以：只在 `persistContent` **成功返回**后更新，且按 `action` 分派
（`written` → set、`deleted` → delete、`skipped` → 不动，见上面代码）；
抛错（`flushContent` 返回 `null`）则什么都不记，下次 `flushAll` 自然重试。

`flushAll` 里保持 `void flushContent(id)` 即可（指纹已在 `flushContent` 内部更新），
**不要**在调用方再写一遍 `.then((o) => persistedFp.set(...))`——那会出现两处真相。
页面冻结时 `persistContent` 的 await 可能来不及完成（iOS 切后台数秒内就停 JS）——
那只会导致 `persistedFp` 偏旧，下次切后台多写一次，方向是安全的（宁可多写，不可漏写）。

这条同样解释了为什么不复用 `cachedAt`：那是「内容来自缓存、尚未被服务端覆盖」的
**UI 标记**（`CacheNotice` 的 `savedAt`），语义与「已落盘」正交，混用会让标注逻辑
与持久化互相污染。

配合上面的空 items 守卫：一个从未打开过的会话既不会产生写事务，也不会占用 LRU
槽位——这才是 §1.1 的真正闭合。

### 4.2 按服务端标识隔离（新增 `lib/cacheScope.ts`）

不把服务端标识编进键名（那要运行期动态化 DB 名、迁移旧库、处理孤儿库），
而是用一个**标记键**记住「当前这批缓存属于哪个服务端」，启动早期比对。

API 分成**同步**与**异步**两半：两种存储性质不同，而列表快照是同步读取的
（`initialListCache` 在渲染期求值，`App.tsx:209`，无法 await）。

```ts
export const CACHE_SCOPE_KEY = "yi-agent.cacheScope";

/**
 * 同步部分：比对 scope，失配就立刻清掉两个 localStorage 键、写入新 scope，
 * 并设模块级「IDB 待清」标志。返回 void——**调用方不需要、也不应该**自己去
 * 触发 IDB 清理（清理由 `createIndexedDbStore` 的 `db()` 单点收口，见下文）。
 * 桌面端（无远端配置）恒为 no-op。
 */
export function syncCacheScope(storage?: StorageLike): void;

/**
 * 配对成功后调用：重新比对一次 scope，**仅在服务端 url 变化时才清**（重配到
 * 同一台电脑不该丢缓存）。与 {@link syncCacheScope} 共用同一段判据，区别只在于
 * 它必须在本次会话内、而不是下次启动时才生效。
 * 返回 Promise 以便调用方/测试 await（内部要清 IndexedDB）。
 */
export function resetCacheScope(storage?: StorageLike): Promise<void>;  // 同一 url 时为 no-op

/**
 * 异步部分：把 IndexedDB `threadContent` 清空。
 * **幂等**（重复调用安全，不报「已空」），但**不吞错**——open/事务失败照常
 * reject，由 `resetCacheScope` 决定是否让整个重配失败（见下文顺序论证）。
 */
export async function clearContentCache(): Promise<void>;

/** 由 `createIndexedDbStore` 的建库入口 await，保证清理先于任何事务。 */
export async function ensureCacheScopeClean(): Promise<void>;
```

流程：

1. `storedRemoteConfig(storage)` 为 `null`（桌面端，或读取抛错）→ 直接返回，
   **不读不写不清**。注意 `storedRemoteConfig` 没有默认参数，`storage` 缺省时取
   `defaultStorage()`，与 `sessionCache.ts` 的既有写法一致。
2. 读 `CACHE_SCOPE_KEY`；与当前 `url`（规范化后）相等 → 无需清理。
3. 不等或缺失 → **同步**删掉 `THREAD_LIST_CACHE_KEY` 与 `SELECTED_THREAD_KEY`，
   写入新 scope，并标记「IDB 待清」。列表快照的读写在 localStorage 上，故此步
   结束后 `readListCache()` 不可能再看到另一台服务端的数据——**串数据问题在同步
   阶段就已闭合**，不依赖任何异步时序。
   顺序上必须**先设标记、再写 scope 键**（两者都在同一个同步块里，中间不会
   被 await 打断，但顺序仍要对）：若先写新 scope 键再设标记，而写键之后的某一步
   抛错，下次启动会判「未失配」却带着一个没被消费的标记——不会出错，但语义
   含糊。与异步的 `resetCacheScope` 不同，这里清的是内存标记 + 同步的
   localStorage，不存在「部分成功」的窗口，风险仅在于可读性。

   **为什么这里可以「先写 scope 键、IDB 稍后清」，而 `resetCacheScope` 必须
   反过来？** 因为两者的容错路径不同，不是双标：

   - 启动路径（`syncCacheScope`）：写 scope 键后若 IDB 清理失败，本次会话的
     `ensureCacheScopeClean` 会 reject，随后所有 `db()` 都失败 → 内容缓存整体
     不可用（读不出旧内容，也写不进新内容）。**旧内容不会再被显示**，所以不构成
     串数据；代价是本次会话没有内容缓存，下次启动 `CACHE_SCOPE_KEY` 已等于当前
     url、不会再清，但上次失败留下的旧条目仍在盘上 —— 这是**唯一的残留风险**，
     且只能通过「内容缓存读不出来」自限（见下一条）。
   - 重配路径（`resetCacheScope`）：同进程内 IDB 早已可用，失败意味着真的清不掉，
     此时若已写 scope 键，下次启动会信任它而带着旧内容继续跑 —— **会串数据**。

   为避免启动路径的残留风险，`ensureCacheScopeClean` 成功清空后**不需要**再写
   任何标记；失败时（reject）应当**把 scope 键回滚为「未知」**（`removeItem`），
   让下次启动重新走一遍失配清理。这是启动路径唯一需要的额外动作，实现时别漏。


4. 标记「IDB 待清」**必须留在模块级状态里**，不能只存在于本次调用的返回值中：
   `syncCacheScope()` 在 App 初始化时调用一次，而 `ensureCacheScopeClean()` 要等到
   第一次 IndexedDB 操作才被调用（可能是几秒之后的 `prefillFromCache`），两者之间
   隔着一次 React 提交。实现上用一个模块级 `let pendingContentClear = false`，
   两侧共享。

IDB 一侧由单一收口点保证顺序，不用调用方配合。但这里有个**容易写错的实现
约束**：`dbPromise` 现在是 `createIndexedDbStore()` 的**闭包局部变量**
（`contentStore.ts:126`），`cacheScope.ts` 根本访问不到它。所以不能写成
「cacheScope 里操作 dbPromise」，必须把「清理已完成」做成**模块级的一次性
Promise**，由 `db()` 在打开前 await：

```ts
// cacheScope.ts —— 模块级，跨 store 实例共享
let scopeClean: Promise<void> | null = null;

export function ensureCacheScopeClean(): Promise<void> {
  return (scopeClean ??= (async () => {
    if (!pendingContentClear) return;
    pendingContentClear = false;
    await clearThreadContentDirect();   // 直接用 indexedDB.open，见下文分层
  })());
}
```

```ts
// contentStore.ts —— createIndexedDbStore 内部
const db = (): Promise<IDBDatabase> =>
  (dbPromise ??= ensureCacheScopeClean().then(open));
```

两个要点：

- **`scopeClean` 必须是模块级、且只 resolve 一次**。`dbPromise` 是每个 store 实例
  一份，而 `createContentStore()` 每次调用都新建一份（`contentStore.ts:167`）；
  目前生产代码只有 `App.tsx:245` 的 ref 缓存这一个创建点，但它**不保证**永远如此
  （测试、未来的多实例场景都可能再建）。清理状态挂在模块级，才不会因为「换了一个
  store 实例」而漏清或重清。
- `dbPromise ??= ensureCacheScopeClean().then(open)` 而不是两步写，避免清理完成与
  建库之间的空档被另一个调用插进来。

于是 `createIndexedDbStore` 的**第一个**事务必然发生在清理之后，而所有内容读写
都经过这个入口（`read` / `write` / `delete` / `keys` 全部走 `run()` → `db()`）。

**调用点必须在 `main.tsx` 的 `ReactDOM.createRoot(...).render(...)` 之前，绝不可
放在 `App.tsx:209` 那一行旁边。** `initialListCache` 虽然用 `useRef(...)` 记忆，
但它的**参数表达式**在每次 render 都会求值（`useRef` 只缓存 `.current`，不缓存
实参），而 `:209` 位于**组件函数体顶层**、不在任何 hook 回调里。把
`syncCacheScope()` 写在那里 = 每次重渲染都跑一遍 `removeItem`/`setItem`，并反复
重置模块级「IDB 待清」标志；`main.tsx` 包着 `<React.StrictMode>`，双次渲染会把
这个问题放大。正确做法是在模块顶层（渲染之前）调用一次：

```ts
// main.tsx —— 早于任何组件渲染，早于 readListCache
import { syncCacheScope } from "./lib/cacheScope";
syncCacheScope();
```

这条是硬约束：**`syncCacheScope()` 是模块初始化期的副作用，不是渲染期副作用。**

另在配对成功后调用 `resetCacheScope()`：配对意味着「换了一个服务端」，不等下次
启动就清干净，避免本次会话内刚配对完的首帧仍画出旧列表。

挂载点是宿主：`App.tsx` 里两处 `onPaired`——首次配对的 `:2292`（只做
`setPairingVersion(v+1)`）与「重新配对」的 `:2316`（`inited.current = false` +
清 repair 状态 + 版本号自增）。两处都要加 `resetCacheScope()`，因为
`PairingScreen` 内部只负责 `saveRemoteConfig`（`:149`），不碰缓存。

一处不对称要注意：`inited.current = false` **只有 `:2316` 有**（`:2292` 是首次
配对，`inited` 本来就是 `false`，`App.tsx:1330` 的守卫从未置位过）。所以不要在
`:2292` 处顺手补一句 `inited.current = false`——那是多余且会误导。两处共同需要的
只有 `resetCacheScope()` 与 `setPairingVersion(v+1)`。

**仅清存储还不够——内存状态同样必须丢（这一条是 §1.2 的真正难点）**：重配不是
冷启动，而是**同一个 App 实例续跑**。`:2316` 的 `onPaired` 只把
`inited.current` 置 false 并 `setPairingVersion(v+1)`，依赖它重跑的 effect
（`App.tsx:1795`）只做 `client.dispose()` + 新建 `RpcClient` + 重新握手，
**不重置** `store`（`useRef(new ThreadStore())`，`:161`）、`warm`、
`cacheMetaRef`、`cachedAt`、`syncPlanRef`。`ThreadStore` 也没有 `clear()`。

后果：清掉 IndexedDB 之后，内存里的旧会话内容仍在（由旧 `store` 持有），侧栏仍
显示旧列表，且 `warm` 会让新服务端上同 id 的会话跳过 `resume`——串数据在内存层
原封不动。

重置清单分两处，**必须都做**：

- **App 层**（`onPaired` 内）：`storeRef.current = new ThreadStore()`（`store` 在
  每个 render 从 `storeRef.current` 重新绑定，故下一次 render 即生效）；清
  `warm` / `cacheMetaRef` / `cachedAt` / `syncPlan` / `persistTimers` /
  `toppedUp` / `subWindow` / `persistedFp`；`inited.current = false`（已有）。
- **`useSessionState` 内的 state**：`currentId`、`groups`、`pinned`、`workspaces`
  是**该 hook 自己的 `useState`**（`useSessionState.ts:65-70`），不在 `store` 里，
  换 `ThreadStore` 不会清它们。`onPaired` 够不到这些 setter，因此需要一条显式
  通道。**本期取「给 `useSessionState` 新增一个 `pairingVersion` prop + 一个
  `useEffect`」**：

  ```ts
  // useSessionState({ ..., pairingVersion })
  useEffect(() => {
    setCurrentId(null);
    setGroups([]);
    setPinned([]);
    setWorkspaces([]);
  }, [pairingVersion]);
  ```

  必须如实说明这不是「零接口改动」：`pairingVersion` 值确实已存在
  （`useSettingsState.ts:55/133`），但它**目前只被 `App.tsx:1795` 的建连 effect
  消费**，`useSessionState` 并不接收它。所以这是**新增一个 prop + 新增一个 effect**，
  而不是「复用现成接口」。工作量小但不可省略。

  另一个必须注意的点：该 `useEffect` 在**首帧也会跑一次**（`[pairingVersion]` 的
  初始依赖），会把 `groups`/`pinned` 从 `initialListCache` 播种的值清空——那恰好
  废掉「首屏画缓存」。必须跳过首次，写法如下（**`useRef` 记首帧，而不是判断
  `pairingVersion === 0`**）：

  ```ts
  const resetOnce = useRef(false);
  useEffect(() => {
    if (!resetOnce.current) { resetOnce.current = true; return; }  // 首帧只做标记
    setCurrentId(null);
    setGroups([]);
    setPinned([]);
    setWorkspaces([]);
  }, [pairingVersion]);
  ```

  为什么不用 `pairingVersion === 0` 判断：该值初值恒为 0
  （`useSettingsState.ts:55` 的 `useState(0)`），单看它确实也能过滤首帧，但它**把
  语义绑在「0 是特殊值」上**——将来若把 `pairingVersion` 持久化（避免每次冷启都
  重建连接），初值就不再是 0，这段代码会静默失效、首帧清空播种值，而且没有任何
  报错。`useRef` 版本只依赖「这是第几次执行」，与具体值无关，不会因此腐化。

为什么不用「重配后整页 reload」偷懒：iOS WebView 里 `location.reload()` 的可用性
与副作用（丢失未落盘草稿、Tauri 注入的运行时是否重建）都需要单独验证，不属于
本期可安全假定的行为。显式重置是确定性的。


**首次升级的代价（明示）**：已在用的手机端没有 `CACHE_SCOPE_KEY`，升级后
首次启动会判为失配并清一次缓存 → 冷启动一次。缓存本就只是占位，正确性优先，
接受这个一次性代价。

**配对成功时为什么要单独调一次，而不能只依赖启动检查**：启动检查已把
`CACHE_SCOPE_KEY` 更新为当前 url，本次会话里再配对到另一台时，若不立即重查，
`readListCache()` 读到的仍是上一台写下的快照，本次会话内就会画出别人的列表。
`resetCacheScope()` 复用同一段判据（url 变了才清），因此「重配到同一台」不丢缓存，
与这里的期望一致。

**`resetCacheScope` 的执行顺序**（两段要分开看，否则会写成矛盾）：

1. **同步段**（第一个 `await` 之前）：清 `THREAD_LIST_CACHE_KEY` 与
   `SELECTED_THREAD_KEY`。不依赖 IDB，立即生效。
2. **异步段**：清 IndexedDB，**成功之后**才写新的 `CACHE_SCOPE_KEY`。

第 2 步的顺序不能反：若先写 scope 键再清 IDB，一旦清理抛错（配额、被系统回收、
open 失败），scope 键已经是新 url，下次启动判「未失配」而保留旧服务器的内容——
**串数据重新出现，且再也不会被自动纠正**。先清 IDB 则相反：失败就整体 reject、
scope 键保持旧值，下次启动仍判定失配并重试（幂等）。所以
`resetCacheScope` 的 `Promise` 必须 reject 而非静默吞错。

注意第 1 步与第 2 步的目标不同：localStorage 侧的**串列表**在同步段就闭合了
（新一帧读不到旧快照），而 IDB 侧的**串内容**要等异步段。这也解释了为什么
`onPaired` 里不需要 await——见下文。

`onPaired` 是同步回调（`() => prefs.setPairingVersion((v) => v + 1)`），拿不到
`await`，所以必须显式接住 rejection，否则会成为 unhandled rejection：

```ts
onPaired={() => {
  void resetCacheScope().catch(() => {
    /* 清理失败：scope 键保持旧值，下次启动会重试。此处不能让配对本身失败，
       否则用户被卡在配对界面而连不上——正确性由下次启动兜底。 */
  });
  prefs.setPairingVersion((v) => v + 1);
}}
```

**顺序注意**：`resetCacheScope()` 是异步的，上面的 `setPairingVersion` 会**先**
执行。这不是 bug——重连 effect 与清理并行进行，而 `readListCache()` 的
localStorage 清理发生在 `resetCacheScope` 内部的**同步段**（第一个 await 之前），
所以新一帧读列表时旧快照已经清掉了。**实现时必须把 localStorage 的清理放在
`resetCacheScope` 的第一个 `await` 之前**，否则这条时序论证不成立。

**`resetCacheScope` 必须自己清 IndexedDB，不能只设标记**：`ensureCacheScopeClean()`
被上面那个模块级 `scopeClean ??=` 记忆化，本会话早已 resolve 过；再设标记不会
触发任何清理。所以它要直接 `await clearContentCache()`（内部同样走 `db()`，与其它
写操作串行）。`clearContentCache` 也**不能**顺手把 `scopeClean` 置回 null——那样
会让下一次 `db()` 又去检查一遍标记，无谓且容易与正在进行的清理打架。它只做
「清空 store」这一件事。

**但这两者不能互相递归**：`clearContentCache()` 走 `db()`，而 `db()` 又 await
`ensureCacheScopeClean()`，后者按设计「有待清标记就清 IDB」。若
`ensureCacheScopeClean` 的实现调用 `clearContentCache`，就成了
`db → ensureClean → clearCache → db` 的自我等待（`dbPromise` 尚未 resolve，
`await` 永久挂起）。因此两者必须分层：

- `ensureCacheScopeClean()`：**直接**用 `indexedDB.open` 拿库并
  `transaction(STORE_NAME).clear()`，不经过 `createIndexedDbStore` 的 `db()`；
- `clearContentCache()`：复用 `createIndexedDbStore()` 的 `db()`（此时
  `ensureCacheScopeClean` 早已 resolve，不会递归）。

这条是本设计里最容易在实现时写错的地方：一旦层级搞反，表现为「手机端内容缓存
彻底不工作且不报错」（所有 `db()` 都 pending）。plan 里要把它写成硬约束，
并配一条「首次操作后 DB 可读」的冒烟测试（jsdom 无 indexedDB，改为手动验收，
见 §5）。

**还要避免模块循环依赖**：`ensureCacheScopeClean` 需要 `DB_NAME` / `DB_VERSION` /
`STORE_NAME` 三个常量，它们现在定义在 `contentStore.ts:120-122`；而
`contentStore.ts` 又要 import `cacheScope.ts` 的 `ensureCacheScopeClean`。直接
互相 import 会形成环。三种可行解，**本期取第一种**：

1. 把三个常量抽到一个新的 `lib/cacheDb.ts`（无任何 import），
   `contentStore.ts` 与 `cacheScope.ts` 各自从它 import。改动最小、方向单一。
   注意它们现在是**模块私有 `const`**（`contentStore.ts:120-122` 没有 `export`），
   抽出去时要加导出；`contentStore.ts` 内部引用处改为 import，不要留下两份定义。
2. 由 `contentStore.ts` 在模块初始化时把常量注入 `cacheScope.ts`
   （`configureCacheDb({name, version, store})`）——多一层可变全局状态，不取。
3. 把 `ensureCacheScopeClean` 挪进 `contentStore.ts`——但 `cacheScope.ts` 的
   `resetCacheScope` 又要用它，环只是挪了个位置。

选 1 的额外好处：常量与 `DB_NAME` 的语义（「这个库归谁」）集中在一处，将来若真要
动态化库名（本期非目标），改一个文件即可。

**判据只用 `url`，不用 `token`**：`RemoteConfig` 是 `{url, token}`（`remoteConfig.ts:18-23`），
但 token 在每次配对时都会重新签发（`saveRemoteConfig` 由 `PairingScreen.tsx:149` 调用，
配对即换）。若把 token 也纳入 scope，同一台电脑每次重配都会判「换了服务端」而丢缓存。
`url` 的形状决定了判据的实际语义：

- 局域网直连形如 `ws://192.168.1.5:8080/ws`（**不含** session，见
  `pair_uri.rs:65-85` 的 fixture）→ 同一台电脑重配，url 逐字相同 → 保留缓存，
  正确。
- 中继形如 `wss://relay.example.com/ws?session=<id>` → 每次配对可能换 session id，
  于是「同一台电脑重配」也会判失配而清一次缓存。**这是已知的误清，接受**：代价
  退化为一次冷启动，而正确性优先（宁可多清，不可串数据）。
- 局域网下 DHCP 换了 IP → url 变 → 判失配 → 清缓存。同样接受：手机此时连的确实
  是「另一个地址」，虽然在物理上可能是同一台机器。

这三条都写进实现注释，避免后人以为 scope 判据是精确的「服务端身份」——它只是
「连接目标的 url」，近似够用。

**为什么覆盖「解绑」**：`clearRemoteConfig` 至今没有 UI 入口，未来若加上，
解绑后 `storedRemoteConfig()` 为 `null`，下次启动走第 1 步 no-op、缓存仍在。
这在本期不改（属非目标「清除缓存 UI」）。真正解决解绑残留的是「下次配到
**不同** url 时失配即清」——而重配到同一 url 时保留缓存是期望行为。

### 4.3 列表快照上界（`sessionCache.ts`）

```ts
/** 非置顶会话最多保留多少条。实测 1000 条约 248–352 KB（取决于路径/标题长度），
 *  相对 5 MB 配额有两位数余量。见 §1.4 的实测表。 */
export const LIST_CACHE_MAX_THREADS = 1000;
```

`writeListCache` 按**全局 `updated_at` 降序**排序后取前 `LIST_CACHE_MAX_THREADS`
条非置顶会话，再把它们按原来的「组 → 组内顺序」重新装回 `groups` 的骨架里——即
**只决定保留哪些会话，不改动渲染顺序**。

`pinned` 的处置要拆成两句话说，否则容易实现错：

1. **不占 1000 条限额**：限额只约束非置顶会话。否则置顶多的时候会把非置顶挤得
   更狠，而置顶本来就是用户显式选的、必须保住。
2. **它们的 id 计入「保留集合」**：置顶项按约定**仍留在各自分组的 `threads` 里**
   （`store.seed` 喂全集，见 `useSessionState.ts:126` 的注释）。若不把置顶 id 加进
   保留集合，`groups[].threads.filter(keep.has)` 会把它们过滤掉，于是「组内成员
   全是置顶」的分组被判成空组而丢弃 —— 冷启首屏少整块目录（`pinned` 分区仍会
   显示这些会话，但 `groups` 侧的表头凭空消失，两侧不一致）。

所以实现是：`keep = new Set(pinned.map(t => t.thread_id))`，再把非置顶按
`updated_at` 降序的前 1000 个 id 并进 `keep`。**判空用 `keep`，限额只数非置顶。**

为什么需要显式排序：服务端 `list()` 产出的是**全局** `updated_at` 降序
（`yi-agent-app-server/src/thread_store.rs:459-463`），但装进 `groups` 后是「按 workspace 分组、组内各自降序」，
组与组之间没有可比顺序。若直接按 `groups` 的遍历顺序截断，某个排在后面的 workspace
里的**全部**会话都会先被砍掉，哪怕它们刚刚更新过——这会让冷启首屏缺掉整个目录。
按全局 `updated_at` 取前 N 条才是「最近 N 条」的真实含义。

真的发生截断时，载荷额外带 `truncated: true`（只写标记，见本节末尾）。

装回骨架时**丢弃被截空的组**：`ThreadSidebar` 渲染 `nested.top.map(...)` 不过滤空组
（`:1904`），留着空组会在冷启首屏画出若干**没有内容的目录表头**。

**`SessionGroup.threadIds` 不需要同步裁剪**（曾怀疑这里会留悬空引用，实测排除）：
`SessionGroup` 用 `threadIds: string[]` 而非 `ThreadSummary[]`（`protocol.ts:249-254`），
但渲染侧有双重过滤——`groupMembers()` 按 `threads` 里现存的对象反查并
「剔除已不存在的 id」（`sessionGroups.ts:15-20`），`layoutWorkspace()` 再跳过成员为
空的框（`:33-35`），`filterThreadTree` 同样用 `present` 集合过滤（`threadSearch.ts:76`）。
所以截断后残留的 id 只是死数据，不会渲染出悬空行。**不裁剪**是刻意的：保留 id 让
「被截断的会话」在下次有完整快照时能自动回到框里，而裁剪是单向破坏。

`truncated` 是**只写**标记（供诊断/测试断言载荷形态），不参与读取决策：
`parseListCache` 显式重建返回对象，故它不会出现在读取结果里。版本号仍为 1
——载荷只多了一个可选字段，既有 v1 读取路径完全不受影响，不存在迁移问题。
是否需要向用户展示「缓存已截断」不在本期范围（属非目标「清除缓存 UI」一类）。

**顺带记录一个与本期无关的既有缺陷**（不在本期修，明确登记避免误导）：`App.tsx:620`
判断 `selectedId` 能否预填时只查 `c.groups.some(...)`，**不查 `pinned`**。因此一个
「只在 pinned 分区、不在任何 group 的 `threads` 里」的选中项不会被预填内容。这
既不是本期截断引入的，也不是本期能修的（要改预填判据，属另一处行为）；本期保留
原样，只在 §2 非目标里登记。

## 5. 测试与验证

基线：`1470 个前端单测 / 116 个测试文件`（`docs/project-management/desktop.md`）。

| 文件 | 新增断言 |
| --- | --- |
| `src/lib/contentStore.test.ts` | 空 items + 默认选项 → `{action:"skipped"}` 且不写入（`keys()` 不含该 id）；空 items + `authoritative` → `{action:"deleted"}` 且条目被删；**只有 `local-*` / notice 时也判为空**（服务端尚未回帧，不得删条目）；非空 → `{action:"written"}` + `fingerprint` 与 `contentFingerprintOf(归一化后 items, …)` 逐字相等（这条是防「两侧指纹不同源」回归的关键）；非空且超 `MAX_ENTRY_BYTES` → 指纹用 **trim 之后**的条目数；现有 15 条不回归（实测：`persistContent` 在 `contentStore.test.ts` 只有 `:155` 一处调用，且未断言返回值，故改签名不破坏既有用例） |
| `src/lib/cacheDb.ts`（新，仅常量） | 无测试价值（三个导出常量），但**必须有**：它是打断 `contentStore.ts ↔ cacheScope.ts` 循环依赖的唯一出口。类型检查（`tsc --noEmit`）即为验证。 |
| `src/lib/cacheScope.test.ts`（新） | **只管 localStorage 与状态机**（jsdom 没有 `indexedDB`，见下方「测试环境限制」）：`syncCacheScope`：scope 相同 → 不清（两个键原样保留）；url 变更 → 两个 localStorage 键立即清空且写回新 scope；scope 缺失（首次升级）→ 同上；桌面端（无远端配置）→ 三类均不动、不写 scope；`getItem` 抛错 → 视为无配置、不抛不清；「IDB 待清」标志置位后，`ensureCacheScopeClean()` **只执行一次清理**（用注入的 `clearThreadContentDirect` 替身计数，不碰真 IndexedDB）。`resetCacheScope`：**同一 url → 不清**；不同 url → 清两个键 + 调用一次清理替身 + 写新 scope；清理替身 reject 时 **scope 键保持旧值**（验证「先清后写」的顺序） |
| `src/lib/sessionCache.test.ts` | 1001 条非置顶 → 落盘载荷保留 1000 条 + `truncated: true`（断言**注入的假 storage**的 `getItem(KEY)` 原文，不经 `parseListCache`——它会显式重建对象，未知字段在读取侧被丢弃）；置顶全部保留且不计入 1000；`≤1000` 条 → 载荷无 `truncated` 字段；**跨组截断**：两个 workspace 各 600 条、一侧 `updated_at` 全高于另一侧 → 保留的是新的那 600 条再加旧的 400 条，且被截空的组不出现在 `groups` 里 |
| `src/App.test.tsx` | (a) 复现 §1.1 的两个实测场景并断言不再发生：12 个空会话写盘后 `real` 仍在；14 条侧栏只读 2 条后两条仍在。(b) **指纹判据**：无内容变更的会话在两次 `flushAll` 之间**只落盘一次**（断言写次数，不是只断言内容在），且内容未变时其 `savedAt` 不前进。(c) **反向漏写边界**：经 `thread/readItems` 补齐、无防抖注册的会话仍会在 `flushAll` 时落盘。(d) **写序不被抹平（限「无流式输出」前提）**：有内容会话多于 `CONTENT_LIMITS.maxCount`（10）、且**没有任何会话在这期间产生 `item/delta`** 时，`flushAll` 只重写指纹变化的那几条，淘汰依据是各自最近一次**写入**的先后，而不是迭代顺序。**必须写明这个前提**——§4.1 已承认脏集（有 `item/delta` 的会话）无条件重写，那种场景下 `savedAt` 仍会被抬平，不能拿(d) 的断言去反证。(e) **重配**：先在与 A 的连接里填满内存 store 与 `warm`，触发重新配对到 B，断言旧会话内容不再渲染、旧 id 不被当作 warm（即 B 上会真的发 `resume`）。<br>测试环境注意：`App.test.tsx` 已 `vi.mock("./lib/contentStore")` 把 `createContentStore` 换成 `MemoryContentStore`（`:118-123`），故内容断言在内存实现上跑；而 `isRemoteClient` **不是** mock 的（只有 `isIos` 被换掉，`:379-388`），要造「手机端」必须往 `localStorage` 写 `yi-agent.remote` 并设 UA，两者缺一不可 |

测试基建的一处必要补充：`MemoryContentStore`（`contentStore.ts:102`，被
`App.test.tsx` 的 `vi.mock` 用作替身）目前不记录调用次数，而上面 (b)(c) 两条要
断言「写了几次」。给它加两个只读计数器（`writes: number`、`deletes: number`，
每次 `write`/`delete` 自增）——它们是测试可观测性，不改变存储语义，也不影响生产
路径（`MemoryContentStore` 只在无 IndexedDB 时兜底）。

**`deletes` 会混入 `evict` 的淘汰删除**：`persistContent` 写完就 `await evict(store)`
（`contentStore.ts:223`），而 `evict` 通过 `store.delete` 清条目（`:186`）。所以
`deletes` 计数 = 「业务删除」+「LRU 淘汰」，两者不可区分。因此：

- 断言「`authoritative` 删了条目」不要用 `deletes === 1`，要用
  **`keys()` 不含该 id**；
- `writes` 是干净的（`evict` 只删不写），但它是**全局累计**，`flushAll` 会为多个
  会话各写一次。断言「某会话只落盘一次」必须先记基线再取差：
  `const before = contentStore.writes; ...flushAll...; expect(contentStore.writes - before).toBe(1)`，
  并且用例里只让**一个**会话有内容变化，否则差值没有意义；
- 若确实要区分两者，再加一个 `evictions` 计数器、由 `evict` 自己递增，
  **不要**和 `deletes` 混用一个字段。本期不强制，`keys()` 断言已足够。

**测试环境限制（已实测，影响测试设计）**：jsdom 27 **不提供 `indexedDB`**
（`typeof window.indexedDB === "undefined"`），而项目**没有** `fake-indexeddb`
依赖，且本期约定不新增依赖。后果：

- `createContentStore()` 在单测里恒走 `MemoryContentStore` 兜底分支
  （`contentStore.ts:167-175`），真实 IndexedDB 路径**完全没有单测覆盖**——
  这是既有状况，本期不改变，也不假装覆盖。
- `ensureCacheScopeClean()` / `clearContentCache()` 的 DB 行为不能靠单测验证。
  因此设计上必须**可注入清理副作用**：`cacheScope.ts` 内部调用一个模块级
  `clearThreadContentDirect`，测试用 `vi.mock` 换成计数替身，断言「被调用了几次、
  顺序如何」；真实清库的正确性靠手机端手动验证（见下）。
- 「首次操作后 DB 可读」这条冒烟测试改为**手动验收**写在 plan 里：手机上清缓存
  后重开 App，确认首屏不白屏、会话内容能正常加载。

**计数器只能加在 `MemoryContentStore` 实例上，绝不能加进 `ContentEntry`**：
`contentStore.ts` 里多处用 `toEqual` 逐字段比对 `ContentEntry`
（如 `applyTrim` 的返回值断言），给 `ContentEntry` 加字段会让既有 15 条用例全挂。若不愿改生产类，可在
`App.test.tsx` 里包一层记录调用的代理对象；二者选其一，本期取前者（更简单，且
顺带让既有用例能断言「没写」）。

验证命令：

```bash
cd desktop && npx vitest run        # 全量
cd desktop && npx tsc --noEmit      # 类型
```

无 Rust 改动，不跑 cargo。桌面端不变性由「所有新分支 gate 在
`isRemoteClient()` / `storedRemoteConfig() !== null`」保证，并由
`cacheScope.test.ts` 的桌面 no-op 用例与既有 `App.test.tsx` 桌面用例锁定。

## 6. 风险

- **清了不该清的缓存**：scope 判据只比 `url`（不含 token），三种 url 形状下的
  实际语义与误清代价已在 §4.2「判据只用 `url`」逐条列出。三种误清都退化为一次
  冷启动，不丢正确性。
- **空 items 误删好缓存**：已用 `authoritative` 收窄到「全量 resume 成功」这一个
  调用点，其余保持不删；测试锁定该边界。
- **启动期 IDB 清理失败导致下次启动信任错误 scope**：`syncCacheScope` 先写
  scope 键、IDB 稍后清；若清理 reject，本次会话缓存整体不可用（`db()` 全失败，
  自然读不到旧内容），但**下次启动**若 `db()` 恢复正常，`keys()` 会读到旧服务端
  的条目，而 scope 键已等于当前 url、不会触发清理 → 串数据。因此
  `ensureCacheScopeClean` 失败时**必须把 scope 键 `removeItem` 回滚**，让下次
  启动重新走失配清理。这条已写进 §4.2，实现时不可省略。
- **清理与预填的竞态**：localStorage 侧由「同步清理」彻底闭合（读之前就已清完）；
  IDB 侧唯一收口点是 `ensureCacheScopeClean()`，所有内容读写都必须经 `run()`
  → `db()`。实现时若绕过 `db()` 直接 `indexedDB.open(...)`，竞态复现——plan 里
  要把它写成硬约束。
- **桌面端不该有任何新行为**：`syncCacheScope()` 在 `main.tsx` 里无条件被调用一次
  （渲染之前），但它在桌面端第一步就返回（无远端配置 ⇒ 直接 return，不读不写不清），
  因此桌面端只是多一次 `localStorage.getItem` 级别的判断，没有任何可观察行为变化。
  注意 `isRemoteClient()` 与 `parseRemoteConfig()` 在
  私密模式的 webview 里会因 `getItem` 抛错而返回 null/false（`remoteConfig.ts:56-61`），
  `syncCacheScope` 必须容忍同样的异常，不得把它当「失配」而清缓存。
- **指纹方案自身的风险：`persistedFp` 与盘上状态漂移**。它是内存里的第二份真相，
  与 IndexedDB 之间没有任何事务保证：进程被杀、写入失败、`deleteCachedContent`
  漏清、冷启播种遗漏，任一情形都会让它与盘上不一致。方向性上两种偏差不对称——
  「指纹偏旧」只会多写一次（安全），「指纹偏新」会静默跳过必要的写（丢缓存）。
  因此所有更新点都必须挂在**成功之后**，且删除路径必须成对清理。`persistedFp` 的
  全部触点共五处：写成功 `set`、`authoritative` 删除 `delete`、冷启播种 `set`、
  `deleteCachedContent` 内 `delete`、`App.tsx:748`（`drop` 分支）内 `delete`。
  这是本期方案的主要复杂度来源，plan 里要逐点列出，缺一即漏。
- **指纹不是内容哈希**：同 id 就地替换且长度与游标不变时判「没变」。已论证这种
  重叠是服务端权威值的幂等重放，与已落盘内容等价（§4.1）。但若将来引入「同 id
  但内容确实不同」的 patch 语义，指纹必须重新设计——这一条要写进代码注释。
- **旧文档被就地修改**：保留一行「本处已由 `<新 spec>` 修正」的指引，避免
  未来读者以为文档与实现对不上是笔误。

## 7. 对旧文档的修正表

本文对 `2026-10-11-mobile-local-cache-design.md` 的修正（同时在该文档内就地
订正，并加一行指回本文）：

| 旧文档位置 | 原陈述 | 订正为 |
| --- | --- | --- |
| 旧 §4.1 | 「纯函数：`serializeCache` / `parseCache` / `pickCacheable`」 | 「纯函数：`parseListCache` / `readListCache` / `writeListCache`（`sessionCache.ts`）」 |
| 旧 §4.1 | 「写入时机：`refreshThreads()` 成功、`handshake` 的 `thread/listAll` 成功后；`selectThread(id)` 时更新 `selectedId`」 | 「写入时机：`thread/listAll` 成功后（握手 `App.tsx:1676` 与 `refreshThreads` `useSessionState.ts:127` 两处）。快照里带 `selectedId`（写入时取当时的 `store.currentId`，不随 `selectThread` 即时更新）；「最后看的会话」另有独立键 `yi-agent.selectedThread`，两者用途不同」 |
| 旧 §8 | 「`serializeCache`/`parseCache` 往返；…`pickCacheable` 的裁剪选择」 | 「`parseListCache` 的坏值/版本/结构校验；`writeListCache` 的截断上界（本文 §4.3）」 |
| 旧 §9 | 「localStorage 超限：列表元数据小，仍有 `pickCacheable` + try/catch 兜底」 | 「localStorage 超限：`writeListCache` 有 1000 条上界（本文 §4.3）+ try/catch 兜底」 |

§5.C 的幽灵来源描述已由 `2026-10-17-client-state-negotiation-design.md` 修正，
本文不重复。
