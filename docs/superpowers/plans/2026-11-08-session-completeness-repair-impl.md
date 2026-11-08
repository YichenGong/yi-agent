# Session 内容完整性修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 不靠发送、切 session 或重启，让普通会话正文、运行状态和当前附属摘要自行收敛；加载失败诚实保留旧视图。

**Architecture:** 显示收敛与数据收敛独立交付。宿主维护带 generation/seq 的可展示 ReadView，提供有界一致快照；客户端只有一个订阅协调器与一个每会话同步控制器，完整基线原子安装后衔接连续实时后缀。5秒可见 probe 发现无后续事件的尾帧丢失。

**Tech Stack:** Rust/Tokio/serde/ts-rs、既有 JSON-RPC/stdio/WS/Tauri、React/TypeScript/Vitest、真实 Chromium/CDP 与隔离 WKWebView/iOS。

## Global Constraints

- 真源：[Spec rev2](../specs/2026-11-08-session-completeness-repair-design.md)、[三轮复核记录](../reports/2026-11-08-session-completeness-spec-review.md)。**本 Plan 未执行，不授权自动部署。**
- 显示单元执行 [显示 Plan A1–A5](2026-11-08-session-display-convergence-impl.md)，不可用本数据单元替代 D01–D09。本文件是总入口及数据单元 B1–B12。
- 执行时新建代码 worktree。先重新对照 main 的相关路径变化，再红测试；不在 main/docs 工作分支写实现。不改正式 `YI_AGENT_HOME`、relay 配对、生产进程或用户正文。
- 显示 initial60/page200/max400/buffer20 不变。v1 内存/帧/重试上限不放宽：队列256、同步缓存256帧/2MiB、完整帧1MiB、单token64MiB/60秒/每客户端2份、宿主共享材料与编码预算256MiB、周期1024页/64MiB/60秒、单RPC10秒、episode120秒/最多4捕获。
- 阅读不发 resume/interrupt/start；显式发送仍遵守 stale_context/epoch。同步重试不自动重发问句、不清草稿附件。
- 不新增依赖；协议先生成TS后接线。旧 readItems/通知 params/远端权限与恒推 List 保持兼容。新能力在**每客户端** initialize 中协商。
- 每任务先运行列出的红/绿测试；提交前 `cd yi-agent-rs && cargo fmt --all && just ci`，前端变更加 tsc/相关Vitest。更新项目进度证据但总 feature 未满足全矩阵前保持 `[ ]`。只有当前文档工作免实现CI。
- 快照材料不能包含模型 messages、reasoning signature/encrypted/redacted data；诊断只记 ID/seq/epoch/count/bytes/timing/reason。

## 任务图与总交付

```
显示： A1 → A2 → A3 → A4 → A5
数据： B1 → B2 → B3 → B4 → B5
       B1 → B6 → B7
       B1 → B8
       B5+B6+B7+B8 → B9 → B10
       B3+B5 → B11
       A5+B10+B11 → B12
```

B2/B6/B8 可在独立 worktree 并行；B3/B4共享服务端发布路径必须串行。App/ChatView 集成（A3/A4/B10）不能由两个实施者并行改同文件。每个 commit 可独立测试，不提前公开 capability 为true；B1 定义/解析能力，B5端到端宿主完成后才开启宿主能力，B10才启用前端选择。

## 冻结接口（后续任务不能另造字段名）

1. 请求 camelCase：read `{threadId,snapshotToken?,cursor?}`、probe `{threadId}`、release `{threadId,snapshotToken,installed?}`。
2. 响应及 envelope.sync snake_case；快照含 Spec §5.2 全部字段（含每页一致的 `source:hot_projection/local_disk/external_disk`，未知活动writer不能先synced再等probe纠正），seq帧额外含 `seq_from,seq_to,subscription_generation,reasoning_item_id?`，所有正文身份包含 `view_generation`。probe 含最新 `view_seq` 与独立摘要，不把多子系统伪装同一事务。
3. capability `thread_transcript_sync_v1`，initialize响应 `transcriptHostInstanceId` 复用宿主已有运行UUID。
4. 新 Rust 模块：`src/transcript/{mod.rs,material.rs,view.rs,outbox.rs,snapshot.rs}`，归属 `yi-agent-app-server`，不是新crate；`src/rpc/thread/content/transcript.rs` 负责只读RPC。
5. 新 TS 模块：`lib/transcriptProtocol.ts` 校验生成类型，`lib/subscriptionCoordinator.ts` 唯一thread/subscribe写入者，`lib/transcriptSync.ts` 控制器，`hooks/useTranscriptSync.ts` 接线。
6. 新同步状态单独放 `ThreadView.transcriptSync`；旧 `syncState` 继续负责发送上下文资格。同步持有记录属于 slot/host/connection/thread，不拿threadId字符串作为全局宿主键。

---

### Task B1：协议、能力与方法归属

**Files:** Modify `yi-agent-rs/crates/yi-agent-app-server/src/{lib.rs,protocol.rs,rpc/mod.rs,rpc/thread/mod.rs,rpc/thread/content/mod.rs,rpc/thread_methods_baseline.txt,server/transport/mod.rs}`; Create `src/transcript/mod.rs`, `src/rpc/thread/content/transcript.rs`; generated bindings `desktop/src/lib/generated/`; Create `desktop/src/lib/transcriptProtocol.ts`, `.test.ts`。

**Consumes:** 现有 NotificationEnvelope/Scope/initialize。**Produces:** TranscriptSyncStamp/TranscriptPage/TranscriptProbe/TranscriptRelease 等 Rust DTO及生成TS；新增3方法仅Observe、可选sync、客户端能力登记。snapshot metadata DTO复用现有usage/info/status类型，显式定义其可空字段；不以 `any` 替代。

- [ ] **Step 1 红测试**：protocol serialization 测旧params逐字JSON相同，sync未协商时字段不存在；RPC归属coverage新增 read/probe/release，thread baseline计数由26→29。TS decoder拒绝负seq、NaN、unsafe integer、from>to、缺generation、不一致页身份。

```ts
import {decodeSyncStamp} from "./transcriptProtocol";
it("rejects a range that cannot be ordered", () => {
 expect(decodeSyncStamp({host_instance_id:"h",thread_id:"t",content_epoch:0,
   view_generation:"v",seq_from:12,seq_to:11,subscription_generation:1})).toBeNull();
});
```

- [ ] **Step 2 验红**：`cd desktop && npm test -- src/lib/transcriptProtocol.test.ts`；`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib owns_coverage`。确认类型齐全后行为红，不将导入缺失当最终判别证据。
- [ ] **Step 3 实现**：导出DTO、解析camelCase请求；保留Notification原形只扩envelope。注册方法分域与权限，不调用旧resume。initialize解析可选请求capability并存client记录，服务端实现未就绪先不advertise true。
- [ ] **Step 4 验绿**：上述测试加 `cargo test -p yi-agent-app-server --lib export_bindings`，检查生成diff，tsc；提交前 `just bindings-check` 与全局门禁。
- [ ] **Step 5 Commit**：`feat: define transcript snapshot and sequence protocol`，只stage本Task文件与对应进度。

### Task B2：严格材料读取与共享修改守卫

**Files:** Create `src/transcript/material.rs`; Modify `src/thread_store.rs`, `src/transcript/mod.rs`; tests放material.rs内及store现有test模块。

**Consumes:** ThreadStore存储路径、meta/log/partial材料。**Produces:** `ReadMaterial {items,metadata,fingerprint,revision,source}` 与结构化 `corrupt_material/snapshot_stale/budget_exceeded`；共享root/thread revision与in-flight mutation guard。不返回messages字段。

- [ ] **Step 1 红测试**：隔离tempdir写合法第一行、损坏第二行、合法第三行；要求错误不是跳过坏行。另测partial/meta坏、append→partial清除中间暂停、truncate原子替换、不同ThreadStore同root共享guard、已有missing空log的合法空会话、预算先准入。

```rust
#[test]
fn strict_material_does_not_accept_a_skipped_committed_row() {
    let line = br#"{"type":"turn","items":[],"messages":[]}"#;
    let mut bytes = line.to_vec();
    bytes.extend_from_slice(b"\n{broken\n");
    assert!(parse_committed_items(&bytes).is_err());
}
```

`parse_committed_items(&[u8]) -> Result<Vec<Item>, MaterialError>` 是本模块新增纯解析入口，DTO的messages用IgnoredAny跳过；测试额外放敏感假字段并断言输出/错误日志均无其内容。
- [ ] **Step 2 验红**：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib strict_material`。补stable guard测试后临时移除revision复核，必须红于混合材料被接受。
- [ ] **Step 3 实现**：共享root规范化/thread登记表为Weak资源，不无限积累死thread。修改路径append、write/clear_partial、truncate、delete、epoch变更前后RAII标记（错误返回也drop）。读取后台I/O前后取revision/in-flight及meta/log/partial文件指纹，不持锁扫日志；最多3捕获，变化返回snapshot_stale。权限错误不能当missing。
- [ ] **Step 4 验绿**：上述测试与 `cargo test -p yi-agent-app-server --lib thread_store`。统计serialized bytes逐步限额而不是无限读再检查；外部writer检测只作external_disk受限证明，不声称多文件事务。
- [ ] **Step 5 Commit**：`feat: read strict transcript materials without model payloads`。

### Task B3：ReadView、稳定reasoning ID与串行提交

**Files:** Create `src/transcript/view.rs`; Modify `src/{translate.rs,session.rs}`, `src/server/{rpc_io.rs,turn/mod.rs}`, `src/server/driver/{turn_driver.rs,launcher.rs}`, `src/server/turn/{command.rs,persist.rs}`；以实际通知生产调用点为准先列清单，缺文件不另造同名模块。

**Consumes:** B1 DTO/B2稳定材料/普通事件。**Produces:** `ReadViewRegistry`，Translator新增 `reasoning_item_id(&self)->Option<&str>` 供v1发布读取，`commit(notification)->PublishIntent`，`capture()->CapturedView`，`PublishIntent {notification,sync,reliability}`；热化复用基线，新投影UUID generation，seq安全溢出拒绝/换代而非回绕。

- [ ] **Step 1 红测试**：首reasoningdelta→snapshot→ThinkingBlock：同ID且遮蔽终项覆盖明文；started/completed/upsert顺序、工具stdout no-op仍消费seq；结束早于append/append失败仍热可读；冷转热不重复items，回收重建generation改变。

```rust
#[test]
fn thinking_finalization_reuses_the_stream_identity() {
    let mut t = Translator::new("t".into(), UsageTotals::default());
    t.on_event(AgentEvent::ReasoningDelta {
        text: "visible thought".into(), kind: yi_agent_core::ReasoningKind::Content,
    });
    let id = t.reasoning_item_id().expect("first delta reserves id").to_owned();
    let out = t.on_event(AgentEvent::ThinkingBlock {
        text: String::new(), token: ThinkingToken::Redacted("TEST-SECRET".into()),
    });
    let final_item = out.iter().find_map(|n| match n {
        Notification::ItemCompleted { item: Item::Thinking { id, text, redacted }, .. }
            => Some((id, text, redacted)), _ => None,
    }).expect("thinking completed");
    assert_eq!(final_item.0, &id);
    assert!(final_item.1.is_empty() && *final_item.2);
    assert!(!serde_json::to_string(&out).unwrap().contains("TEST-SECRET"));
}
```

在translate测试模块使用已有AgentEvent/ThinkingToken/Notification imports；getter仅暴露展示ID，不暴露token。旧on_event仍返回Notification，v1发布层用getter填sync；后续多个reasoning块须各自分配/终项重用。
- [ ] **Step 2 验红**：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thinking_finalization`，保留旧ID分配导致不等的日志。并发两个producer barrier测试复核发布顺序。
- [ ] **Step 3 实现**：共享不可变历史+活动item可变投影，只可展示状态；apply→seq→有序发布意图同短临界区，无network await。补通知来源清单，覆盖Spec §5.6全部seq集合与metadata设置/已读变更。Translator首delta预留ID最终重用；旧params不扩itemID。read capture同时seal现有coalescer，后续新range从snapshot_seq+1起。
- [ ] **Step 4 验绿**：translate/readview/driver相关crate测试，临时回退首deltaID分配和提交序列化，各自红后恢复。核查未把热driver强制迁出或替换执行writer锁。
- [ ] **Step 5 Commit**：`feat: project ordered live transcript views`。

### Task B4：有界出口、失效水位与合并delta

**Files:** Create `src/transcript/outbox.rs`; Modify `src/broadcast.rs`, `src/server/rpc_io.rs`, `src/server/driver/launcher.rs`, `src/server/transport/{stdio.rs,scoped.rs,mod.rs}`（修改既有出口泵，不新造websocket.rs）；tests在broadcast/outbox及server模块。

**Consumes:** B3 PublishIntent，现队列256、Feed/Delivery筛选。**Produces:** 同一客户端/thread有序出口；dirtyThrough合并失效；可查Full/Closed/coalesced/invalidated计数。

- [ ] **Step 1 红测试**：停读慢local/WS队列，连续完成帧加delta，另快端读与interrupt/probe；所有操作设有界timeout，必须快端/取消不等慢端。恢复读后完整项或invalidated，提示再次Full不消失，installed ACK只能清≤snapshotSeq水位。旧完成溢出明确disconnect，不静默online。
- [ ] **Step 2 验红**：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib transcript_outbox`；在固定容量测试中不用sleep赌满，以填满确认/barrier控制。
- [ ] **Step 3 实现**：同步提交非阻塞，出口泵唯一等待网络者。禁止无限二级队列/每publish无上限spawn；队列及在途计入预算。local v1 dirty元信息至多16thread，All更大合并连接级invalidated；WS满/停滞10秒关闭。旧List关键背压保留，但新增完成路径无业务等待，无法准入明确关闭。反向审批和reply不越过既有顺序屏障。coalescer只合并相同thread/epoch/generation/item的连续range；snapshot seal不等待网络。
- [ ] **Step 4 验绿**：新测试与broadcast历史tests；原测试若要求旧关键帧等待，保留兼容分支测试，v1断言恢复路径。回退completed至try_send时R01必须红，不只发现deltaFull计数。Closed须清client/token/subscription而非无限等待。
- [ ] **Step 5 Commit**：`fix: bound transcript delivery and signal recoverable gaps`。

### Task B5：一致分页token、probe与释放RPC

**Files:** Create `src/transcript/snapshot.rs`, `src/server/tests/transcript.rs`; Modify `src/transcript/mod.rs`, `src/rpc/thread/content/transcript.rs`, `src/server/tests/mod.rs`, `src/rpc/mod.rs`, `src/server/transport/mod.rs`。

**Consumes:** B2/B3 capture+B4 seal+authorization/client身份。**Produces:** immutable token registry、UTF8整帧分页、constant-file-stat冷probe、owned后台任务与release ACK。read完成后token直到release/TTL保留。

- [ ] **Step 1 红测试**：Harness::new同步构造，使用现 `rpc(&mut h,id,method,params).await` 和start_thread。读第一页后更新热正文，再读续页，所有页metadata/seq/items固定；跨client/thread token拒绝。空页、CJK 1MiB、单item过大、TTL、双token/总内存、delete/clear取消token、释放幂等、超时任务清理、Observe可读无控制动作。

```rust
// 添加到 server/tests/transcript.rs，使用该目录现 Harness/rpc/start_thread helper。
#[tokio::test]
async fn transcript_read_is_observe_only_and_does_not_resume() {
    let mut h = Harness::new();
    let id = start_thread(&mut h).await;
    let page = rpc(&mut h, 20, "thread/transcript/read", serde_json::json!({"threadId":id})).await;
    assert!(page["result"]["items"].is_array());
    let probe = rpc(&mut h, 21, "thread/view/probe", serde_json::json!({"threadId":id})).await;
    assert!(probe["result"]["active_turn_id"].is_null());
    assert_ne!(probe["result"]["status"], serde_json::json!("running"));
}
```

Harness不暴露threads字段，使用真实probe读取运行状态，不加入生产测试旁路。再用独立scope Harness测Observe与运行中读取前后turn_id/状态不变；上例仅不发起回合判据。
- [ ] **Step 2 验红**：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server::tests::transcript`。FIFO/捕获barrier测试同时interrupt快速回应，证明不占RPC读取循环。
- [ ] **Step 3 实现**：参考 `rpc/side.rs` owned后台任务：校验权限/身份后clone registry/responder，spawn_blocking只做严格磁盘I/O，不spawn借用AppState。准入后共享capture，锁外编码全JSON-RPC frame核bytes；cursor opaque绑定token，不续页reload。释放installed只清覆盖dirty水位；冷probe常数stat检查generation，读取错误不伪造完整。disconnect/TTL回收任务与token，不回收仍有driver/token的投影。宿主v1全链就绪后advertise capability。
- [ ] **Step 4 验绿**：RPC矩阵、新模块、旧readItems/subscribe tests、bindings-check；回退续页capture→reload必须使页一致测试红。总预算统计共享材料只计一次，编码reservation失败退出不安装/不杀agent。
- [ ] **Step 5 Commit**：`feat: serve bounded immutable transcript snapshots`。

### Task B6：监听ready、只读timeout与envelope透传

**Files:** Modify `desktop/src/{tauriTransport.ts,tauriTransport.test.ts,wsTransport.ts,wsTransport.test.ts,transportFactory.ts}`, `desktop/src/lib/{rpc.ts,rpc.test.ts,protocol.ts}`; Modify `desktop/src-tauri/src/bridge.rs`及其既有测试。

**Consumes:** B1 sync DTO、Transport。**Produces:** `Transport.ready?():Promise<void>`（旧test transport可立即就绪），`RpcClient.ready():Promise<void>`；request第三参数 `{timeoutMs?:number,dropLateResponse?:boolean}`，仅只读控制器使用10秒；raw通知保sync，未订阅请求响应仍正确关联。

- [ ] **Step 1 红测试**：deferred Tauri listen未完成时send不能invoke，listen reject让ready/request失败不是只console；dispose后listen迟到立即unlisten。fake clock timeout移除pending，迟到read响应不无限buffer、不影响下一请求；普通start不自动重试/增加timeout。bridge失败emit计数+断线/前台probe保底。
- [ ] **Step 2 验红**：`cd desktop && npm test -- src/tauriTransport.test.ts src/wsTransport.test.ts src/lib/rpc.test.ts`。
- [ ] **Step 3 实现**：onMessage/notification缓存同步登记，ready等待全部所需listen与WSopen；失败清理。dropLateResponse用有界tombstone（最多256/60秒TTL）或本client单调已过期ID范围，禁止新无限Set。dispose reject/clear timers。bridge不重组丢sync，不把管理secret/relay参数打进日志。
- [ ] **Step 4 验绿**：单测/tsc，`cargo test --manifest-path desktop/src-tauri/Cargo.toml --lib bridge`；真实链验证留B12，不能拿mocklisten代R03。
- [ ] **Step 5 Commit**：`fix: await transport listeners and bound read requests`。

### Task B7：唯一订阅协调器

**Files:** Create `desktop/src/lib/subscriptionCoordinator.ts`, `.test.ts`; Modify `lib/subscriptionWindow.ts`, `App.tsx`旁问/普通/recovery写入处，`hooks/useSubagentTrace.ts`或现轨迹订阅持有入口（只合并thread filter，不替代trace watch自身串行控制）。

**Consumes:** B6 ready/每客户端source；普通LRU、旁问parent、trace thread hold。**Produces:** `setVisible(threadId)`, `hold(threadId,owner):()=>void`, `ready():Promise<SubscriptionAck>`, `disconnected()`, `dispose()`；每client一条replace链，ACKgeneration所有消费者共享。

- [ ] **Step 1 红测试**：普通A→旁问B→traceC同时持有得到A/B/C而非只B；本地保持mode:all；远端≤16先裁无hold LRU，超hold拒绝新请求并保旧选择。deferred第一ACK期间变更意图不能超车，失败链可恢复；ACK前frame已有缓存。
- [ ] **Step 2 验红**：`cd desktop && npm test -- src/lib/subscriptionCoordinator.test.ts src/lib/subscriptionWindow.test.ts`；App旁问兼容tests必须仍绿。
- [ ] **Step 3 实现**：hold引用计数/幂等release、整体replace串行且owner核对，listener/cache在请求前建立。newhost local协商mode all，oldhost local不发送新mode；remote ACK后才捕获read。删除独立 `[parentId]`替换、recovery自主重订阅，统一经协调器。
- [ ] **Step 4 验绿**：测试/tsc；production `grep -rn '"thread/subscribe"' desktop/src/`仅协调器发请求（协议声明不算）；检查trace watch/unwatch不误停children/process。
- [ ] **Step 5 Commit**：`refactor: coordinate thread subscription ownership`。

### Task B8：Session原子基线安装与本地overlay

**Files:** Modify `desktop/src/lib/{session.ts,session.test.ts,threadStore.ts,threadStore.test.ts,sessionCache.ts,sessionCache.test.ts}`。

**Consumes:** 完整B1 page材料/同步身份，现Notice/Item/Reasoning。**Produces:** `Session.installTranscript(items,metadata):void`、`addUserMessage(...):string`返回local提交ID、`bindSubmission(localId,serverId):void`；新增ThreadView正文状态与可信基线标识，trimmed缓存失去完整证明。

- [ ] **Step 1 红测试**：权威老items安装到有live尾视图，要求按服务端顺序而非尾append；overlay锚ID/删除退尾；旧历史相同问句不认领新local，start/interject显式绑定才认领；草稿/附件/returnedInterjections不丢；redacted权威覆盖长明文；metadata清Stop/旧retry，用量按水位而非旧live优先逻辑。
- [ ] **Step 2 验红**：`cd desktop && npm test -- src/lib/session.test.ts src/lib/threadStore.test.ts src/lib/sessionCache.test.ts`，取真实Session而非替代upsert。
- [ ] **Step 3 实现**：把普通server items与本地overlay责任区分，先validate全部ID唯一/metadata合法再一次replace，bump structuralVersion且reset正确live指针。notice记录preceding serverID，未知提交保留，不文本匹配删除；v1 reasoning按syncID更新。cache旧字段unknown、裁剪baselinePresent=false；持久化scope仍按宿主绑定，安装不触发markRead/approval。
- [ ] **Step 4 验绿**：新tests/旧reasoning/interjection/usage tests/tsc。重复安装同基线不重复overlay，切clear reset新正文身份，不放宽旧syncState发送约束。
- [ ] **Step 5 Commit**：`feat: install authoritative transcript baselines atomically`。

### Task B9：同步控制器与连续后缀

**Files:** Create `desktop/src/lib/transcriptSync.ts`, `.test.ts`, `desktop/src/test/transcriptSyncFixtures.ts`; Create `hooks/useTranscriptSync.ts`, `.test.tsx`。

**Consumes:** B5 RPC+B6ready+B7subscription+B8session安装；source身份、visibility和通知。**Produces:** `TranscriptSyncController {open,accept,invalidate,probe,retry,connectionChanged,dispose}`；依赖注入request/install/apply/state callbacks和clock，生产使用真实RpcClient/Session。

- [ ] **Step 1 红测试**：受控deferred page/fake timer覆盖seq before/after/partialoverlap、dirty期间再dirty、overflow、generation变更、5秒尾probe、4tries后新seq不重置预算、hidden/dispose清timer、release installed真假。fixture定义read/probe响应遵守B1类型，UTF8计算CJK。

```ts
export function classifyRange(applied:number,from:number,to:number) {
 if (to <= applied) return "duplicate";
 if (from === applied + 1) return "apply";
 return "repair";
}
// 添加到 transcriptSync.test.ts；这是新增纯入口的测试，不替代控制器时序测试。
it("never guesses the suffix of a partially covered delta", () => {
 expect(classifyRange(10,8,12)).toBe("repair");
 expect(classifyRange(10,11,14)).toBe("apply");
});
```

- [ ] **Step 2 验红**：`cd desktop && npm test -- src/lib/transcriptSync.test.ts src/hooks/useTranscriptSync.test.tsx`。定义空壳接口后要求关键失败来自seq/dirty/timeout行为，不只编译错误。
- [ ] **Step 3 实现**：同步建缓存→ready/ACK→同token所有页→校验总量/身份/唯一ID→原子安装→连续后缀。snapshot期间临时可见应用不冒充基线；overflow不安装半份。dirtyThrough+local invalidation revision覆盖证明，无finally清dirty。seq重复drop/连续apply/缺口repair，不slice重叠字符串；unknown List只更新列表+dirty。release finally best-effort，installed仅完整有效安装时true。单flight并合并reason，initial+1/2/5秒共4次/episode120秒；只有成功/新连接/显式retry重置。external_disk显示stale受限而非synced。
- [ ] **Step 4 验绿及变异**：回退seqguard→S05红；overflow静默clear→S07红；关闭visible5sprobe→S04红；generation核对→S08红。恢复全绿/tsc；fake timer结束 `vi.getTimerCount()`为0，页面数/byte/try预算逐项断言。
- [ ] **Step 5 Commit**：`feat: reconcile transcript baselines with live sequence ranges`。

### Task B10：App/通知/缓存/发送接线与旧宿主降级

**Files:** Modify `desktop/src/App.tsx`, `App.test.tsx`, `hooks/{useNotificationRouter.ts,useSessionState.ts,useHandshakeRecovery.ts}`, `lib/connectionLifecycle.ts`, `components/ChatView.tsx`；扩既有测试而非另建重复App mock。

**Consumes:** A3滚动归属+B7唯一订阅+B9控制器；initialize能力、连接source。**Produces:** 普通内容唯一v1入口；非阻断stale/retry提示；metadata同步与显示/发送资格分离；legacy受限路径。

- [ ] **Step 1 红测试**：真实App+MockRpc先live尾再coldrunning点击，要求历史全部有且read不带游标、resume/interrupt零调用；框内新问句与旧相同，snapshot到达后草稿/气泡正确；A→B→A两宿主迟到不污染；有历史read失败保旧并stale而非synced，retry不进入forceResume。能力false提示受限、不接受伪seq。
- [ ] **Step 2 验红**：`cd desktop && npm test -- src/App.test.tsx src/hooks/useNotificationRouter.test.tsx`（按既有router测试布局选择，不存在时新建并注册真实hook）确认cold游标案例红。
- [ ] **Step 3 实现**：新连接source登记在wireClient前，capability协商后启用v1；router先交普通同步控制器，UI/side/trace/审批保持原入口，markread/list refresh只执行一次。v1 select替代warm/toppedUp/catchingup/inWindow早退与readItems拼接，不把items非空作synced。send读取addUserMessage返回localID，把start回包绑定 `user-${turn_id}`、interject绑定interjection_id，不自动重发。冷idle阅读不resume；**显式发送**如执行上下文未attach，保留既有控制准备/epoch校验后才start，不能用正文synced作agent已就绪。旧宿主coldrunning完整readItems无after，标能力受限，不能承诺活动流式精确snapshot。
- [ ] **Step 4 验绿**：App/session/router/cache/connection tests、tsc、build、A组旧browser回归。列出所有生产readItems/subscribe调用，每个明确v1替代或legacy用途，禁止漏旧抢写分支。
- [ ] **Step 5 Commit**：`fix: route session reads through transcript reconciliation`。

### Task B11：附属摘要权威水位与独立freshness

**Files:** Modify Rust `src/{rpc/agent.rs,rpc/process.rs,server/watchers.rs,transcript/view.rs,rpc/thread/content/transcript.rs}`；Modify TS `hooks/{useProcessState.ts,useSessionState.ts,useTranscriptSync.ts}`, `lib/subagents.ts`, `components/{SubagentRail.tsx,ProcessRail.tsx}` 与对应tests。

**Consumes:** 既有daemon ListTaskSummaries、process manager权威状态、B5probe。**Produces:** includeSync可选列表返回sync、独立revision/freshness/checked_at；前端摘要singleflight+request generation。

- [ ] **Step 1 红测试**：daemon失败保持旧摘要stale而非fresh空表；通知丢失后probe revision更高只拉相应列表；process未知thread v1 source_unavailable，legacy仍旧shape。A→B→A旧列表与旧client回包不覆盖。相同内容不递增revision，输出read timer不叠加。
- [ ] **Step 2 验红**：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib transcript_summary`；`cd desktop && npm test -- src/hooks/useProcessState.test.tsx src/hooks/useSessionState.test.tsx`。
- [ ] **Step 3 实现**：复用500ms watcher权威源，阻塞IPC放后台blocking任务/有界超时，不虚构daemon轻量probe API。最近成功快照+fingerprint生成revision，失败只改freshness，未知不能与空表等同。读取中再invalidated保留pending revision，列表安装同时校验source/request generation/revision。普通正文失败不清摘要，摘要失败不重读正文。
- [ ] **Step 4 验绿**：watcher/list/process与hook测试；既有trace unwatch不停止兄弟watcher回归；tsc/build。5秒probe只当前可见，不给侧栏每threadtimer；hidden/resume按B9可见调度。
- [ ] **Step 5 Commit**：`fix: reconcile independent session summary freshness`。

### Task B12：端到端压力、变异、真机门禁与收尾

**Files:** Expand `src/server/tests/transcript.rs`及既有stdio/WS测试；Create `desktop/scripts/browser/session-sync.{html,tsx}`, `session-sync-check.mjs`、必要的隔离transport fixture；Modify `desktop/package.json`, `desktop/scripts/browser/README.md`, `docs/testing/session-completeness.md`（由A5创建）、项目模块/索引/bug-list；bridge实际链路测试扩现有测试。

**Consumes:** A5+B10+B11真实链。**Produces:** D/S/R逐ID可追溯结果、变异红日志、独立真机报告，不含生产资料。

- [ ] **Step 1 红端到端用例**：真实stdio子进程/stdout pump、Tauri forwarding/emit、真实RpcClient/Session；另真实WS含relay透传组合。受控丢帧只在测试transport门，不能改生产代码绕过广播。压力大工具输出同时read/probe/interrupt，assert无切会话/无新增输入下正文最终等于权威、ID唯一、状态正确、快端/Stop不等待慢端。fixture事件有明确范围与token，不只mockBroadcaster。
- [ ] **Step 2 执行S/R矩阵**：新增 `npm run test:browser:session-sync`，独立Vite1453/CDP9363、profile、测试host与绝对临时YI_AGENT_HOME，无真实LLM；缺Chrome失败、所有自己spawn进程finally清理。每条记录commit/ID/seq/count/bytes/reason，不写正文/token。新脚本须保留生产App的接线，pure控制器测试不能代S组App或R03桥测试。
- [ ] **Step 3 判别性变异**：依次coldread恢复after游标(S01)、去seqguard(S05)、overflow不dirty(S07)、禁visibleprobe(S04)、完成退try_send(R01)，每处运行对应真实层失败，再恢复并确认源文件sha一致。D01/D05由A5报告引用；不得全量回退把不相关失败当红证据。
- [ ] **Step 4 全绿门禁**：`cd desktop && npm test && npx tsc --noEmit && npm run build && npm run test:browser:session-completeness && npm run test:browser:session-sync && npm run test:browser:session-view && npm run test:browser:transcript-window && npm run test:browser:stream-live`；`cargo test --manifest-path desktop/src-tauri/Cargo.toml --lib`；`cd yi-agent-rs && cargo fmt --all && just ci`。报告必须列全部26 IDs，不用历史464passed代替。
- [ ] **Step 5 独立真机与收尾Commit**：得到用户测试安装授权后依CLAUDE独立bundle/绝对YI_AGENT_HOME/前端先build/正确三层签名流程。WKWebView D01/D05/D06/D07+stdio压力；iOS/WS/relay S04/S05/S08弱网、前后台、重连；不改原配对。没条件测的行保持 `[ ]`，不能总体声称完成。提交 `test: verify end-to-end session completeness recovery`。全部门禁满足且用户决定合并后，走finishing skill及no-ff合并、合并结果重测；未获合并授权不自动merge/push。

## 覆盖索引与自审

| 验收 | 任务 | 关键证明 |
| --- | --- | --- |
| D01–D09 | A1–A5 | 原生滚动/真实几何与有限调度 |
| S01 | B2/B5/B9/B10/B12 | coldrunning无live游标/无控制动作 |
| S02/S03 | B3/B8/B9/B12 | 缺口修完整items/同ID全文 |
| S04 | B5/B9/B10/B12 | 无后续事件5秒probe修尾 |
| S05 | B3/B4/B5/B9/B12 | seal、snapshot边界、连续seq |
| S06 | B2/B5/B9/B12 | immutable token/全部资源上限 |
| S07 | B4/B9/B12 | dirtyThrough、overflow、有限重试 |
| S08 | B2/B3/B5/B6/B7/B9/B10/B12 | 宿主/连接/epoch/generation隔离 |
| S09 | B8/B10/B12 | 显式ID认领/草稿保留/不自动重发 |
| S10 | B1/B7/B8/B10/B12 | 能力协商/旧缓存与旧协议受限 |
| S11 | B2/B3/B5/B9/B12 | 热投影不假定append成功/重启换代 |
| S12 | B11/B12 | 权威摘要/revision/迟到防回退 |
| R01/R02 | B4/B5/B12 | 有界可靠或明确恢复/断线 |
| R03 | B6/B10/B12 | 真实stdio/Tauri/WS/relay透传 |
| R04 | B9/B11/B12 | timer/预算/singleflight清理 |
| R05 | B2/B4/B5/B6/B12 | 日志白名单，无敏感载荷 |

本文件是实施契约而非复制全部实现源码。Task中的新接口、测试helper必须先按所列真实模块补齐，红证据最终须指向行为而非未定义符号。原通知生产路径和server test helper随main漂移时先对照并修Plan，不凭猜测新造RPC布局。所有可选降级限制已明确；任何实施中无法维持的预算、冷材料证明或出口顺序都应回到Spec重新评审，不自行缩减矩阵。
