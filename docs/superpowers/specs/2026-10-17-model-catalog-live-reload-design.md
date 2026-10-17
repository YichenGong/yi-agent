# 设计：模型清单运行期热重载（daemon 与 app-server 的清单快照失效）

日期：2026-10-17
状态：待评审
关联：`2026-10-09-model-catalog-authority-design.md`（清单权威链语义）、`2026-10-15-openai-base-url-and-model-test-design.md`（`/v1` 语义与「不隐式改写」的立场）、`2026-10-01-desktop-runtime-self-heal-design.md`（daemon 探活/接管先例）

## 1. 背景与问题

### 1.1 直接触发：改好 `models.json` 后 subagent 仍 100% 失败

现场事实（2026-10-17，本仓库）：

- 13:40:01 启动的项目 runtime daemon（PID 23137）与 13:40:03 启动的 app-server（PID 23183）都读到了当时那份 `~/.yi-agent/models.json`：openai 条目的
  `api_url = "https://llmapi.horizon.auto"`（**不带 `/v1`**）。
- 13:45:38 用户把三条条目的 `api_url` 改成 `"https://llmapi.horizon.auto/v1"`。
- 此后所有 subagent 委派全部失败，落库状态一律 `failed`，trace 事件为：

  ```
  {"type":"state_note","note":"failed: provider error: server error: unexpected status 404: "}
  ```

- 定量对照（同一二进制、同一份已修好的清单，只换 daemon 进程）：

  | 进程 | 启动时刻 | 200 OK | 404 |
  |---|---|---|---|
  | 旧 daemon 23137 | 13:40:01（快照带 `/v1` 修复**之前**的值） | **0** | **120** |
  | 新 daemon 3132 | 17:30:30（快照带 `/v1` 修复**之后**的值） | **2** | **0** |

- 现场实测 URL 语义，排除「网关故障」：`POST https://llmapi.horizon.auto/chat/completions`
  → 404；`POST .../v1/chat/completions` → 200；`POST .../v1/responses` → 200。

即：**配置已经修好，但已运行的进程永远看不见**。用户唯一的自救手段是「杀掉并重启 daemon」，
而这条路径既不可发现（无任何提示），也不是产品该有的行为。

### 1.2 根因：两处「启动时快照一次清单」

清单解析本身没有错，错在**解析结果被进程生命周期冻结**。

**（一）daemon 侧，冻结点是 provider 本体。**
`yi-agent-subagent/src/attach.rs:110` `build_worker_factory`：

```rust
let effective = yi_agent_runtime::models::resolve_effective(cfg, catalog, catalog.subagent_model.as_deref());
let provider = yi_agent_runtime::bootstrap::build_provider(effective)?;   // ← attach.rs:125
```

`provider` / `agent_config`（含 `model`、`reasoning_effort`）/ `sandbox` 都由这一次
`effective` 派生，随后永久存放在 `DaemonAgentWorkerFactory` 的字段里
（`lib.rs:50-70`）。worker 启动时只是克隆：

```rust
let provider = Arc::clone(&self.provider);   // lib.rs:563
```

`attach.rs:137` 是 daemon 侧**唯一**的非测试构造点。全仓 `DaemonAgentWorkerFactory::new`
共 23 处：这一处生产点 + `lib.rs` 的 21 处 + `tests/subagent_fork_e2e.rs:148` 的 1 处，
后 22 处全在测试内。所以这个冻结点是全项目 runner 的单一来源。

**（二）app-server 侧，冻结点是清单快照。**
`yi-agent-app-server/src/server.rs:1421`：

```rust
// 清单在启动时读一次;`production_factory` 的闭包持有它,逐会话按 `model_ref` 解析。
let catalog = Arc::new(yi_agent_runtime::models::load_catalog());
```

闭包每次**确实**重建 provider（`bootstrap.rs:320` `build_provider` 在 `bootstrap_agent` 内），
但它拿到的 `catalog` 是启动快照，于是**运行期新增的条目**（设置页 `model/upsert`）
对它不可见——`server.rs:3521` 的注释已自认此事：

> `production_factory` 只在进程启动时快照一次清单,运行期经 `model/upsert` 新增的条目不在其快照内

app-server 目前靠 `thread/setModel` 分支（`server.rs:3956`）**运行期重读磁盘**来兜住主对话；
但 `thread/start` / `thread/resume` 走的是工厂闭包，所以「在设置页新增模型后不切模型、
直接开新会话」拿不到新条目。这是与 daemon 同源的独立缺陷。

### 1.3 与既有设计的关系（为什么不能「自动补 /v1」）

`2026-10-15-openai-base-url-and-model-test-design.md` §5.1 已明确**拒绝**「末尾没有版本段
就补 `/v1`」的隐式改写，理由是 `/api`、`/openai`、`/proxy` 这类前缀无法可靠判断。
本次**沿用该立场**，不动任何地址改写逻辑：用户填网关真实地址，UI 侧由既有的
「OpenAI 兼容接口地址通常以 `/v1` 结尾，请确认」提示兜住。

本次要修的是**另一件事**：配置改对了，进程没跟上。

## 2. 目标与非目标

### 目标

1. `models.json` 变更后，**已运行的 daemon 与 app-server** 在下一次用到清单时自动采用新值，
   不需重启进程。
2. 变更来源无关紧要：手工编辑文件、TUI/CLI 侧写入、设置页 `model/*` RPC —— 一视同仁。
3. 坏文件**不阻断**：解析失败时保留上一次可用清单并记 trace，修好后下一次立即生效。
4. 零回归：未装刷新器的 factory（全部测试构造点）行为逐字节不变。

### 非目标

- **不改 openai base_url 语义，不加隐式 `/v1` 改写**（§1.3）。
- **不覆盖 `.env`**。`.env` 是兜底层，按 `2026-10-09-model-catalog-authority-design.md` §5
  「显式接管」语义，导入后它不再跟随；且 `cfg` 承载 provider/model/max_turns 等启动期参数，
  运行期穿透会扩大面而无对应收益。
- **不改 `provider_profile_id()`**（见 §6.2）。
- 不做跨进程通知协议（daemon 与 app-server 各自按指纹自更新，不互相喊话）。
- 不做定时轮询（对「下次委派才用到」的场景无收益）。

## 3. 术语

- **清单指纹（fingerprint）**：`models.json` 的 `(mtime, size)`；`None` 表示文件不存在。
- **last-known-good**：最近一次**解析成功**的清单。
- **严格版读取**：区分「文件不存在」与「文件损坏」的读取，见 §4.1。
- **派生件（WorkerParts）**：由 `effective` 派生出的一组不可变对象
  （provider / tools / agent_config / sandbox / writable_roots）。

## 4. 设计

### 4.1 新增严格读取 `try_load_catalog_from`

现状 `load_catalog_from`（`yi-agent-runtime/src/models.rs:122`）把**所有**失败都吞成空清单：

```rust
Err(error) if error.kind() == NotFound => return ModelCatalog::default(),
Err(error) => { warn!(...); return ModelCatalog::default(); }   // 损坏与「空」无法区分
```

它的容错语义（「坏文件绝不阻断启动」）必须保留，因此**不改它**，新增一个严格版：

```rust
/// 严格读取：区分「文件不存在」（Ok 空清单）与「不可读 / 不可解析」（Err）。
/// 既有 `load_catalog_from` 保持「绝不报错」的容错语义不变。
pub fn try_load_catalog_from(path: &Path) -> Result<ModelCatalog, String>
```

对照表：

| 磁盘情况 | `try_load_catalog_from` | 是否推进指纹 |
|---|---|---|
| 文件不存在 | `Ok(空清单)` | 是（回退 cfg 是明确意图） |
| 内容合法（含合法空清单） | `Ok(catalog)` | 是 |
| 不可读 / 不可解析 / 半截写入 | `Err(reason)` | **否**（下次重试） |

### 4.2 新增共享组件 `CatalogWatcher`

daemon 与 app-server 共用同一实现，避免两套新鲜度逻辑：

```rust
// yi-agent-runtime/src/models.rs
pub struct CatalogWatcher { path: PathBuf, state: Mutex<State> }
struct State { fingerprint: Fingerprint, catalog: Arc<ModelCatalog> }
struct Fingerprint { mtime: Option<SystemTime>, size: u64 }

impl CatalogWatcher {
    /// 构造即读一次并记指纹（用于进程启动后的首次解析）。
    pub fn new(path: PathBuf) -> Arc<Self>;
    /// 固定快照：用于测试与「本就持有清单」的调用方，永不重读。
    pub fn from_catalog(path: PathBuf, catalog: ModelCatalog) -> Arc<Self>;
    /// 指纹相同 → 直接返回缓存 `Arc`；不同 → 重读，成功才推进指纹，失败保留旧值。
    pub fn current(&self) -> Arc<ModelCatalog>;
}
```

`current()` 的精确语义：

1. `stat(path)` 得 `Fingerprint`。
2. 与缓存指纹相同 → 返回缓存 `Arc<ModelCatalog>`（**零解析**）。
3. 不同 → `try_load_catalog_from`：
   - `Ok(catalog)` → 替换缓存 + 推进指纹 → 返回新 `Arc`；
   - `Err(reason)` → `tracing::warn!` + **保留旧缓存、不推进指纹** → 返回旧 `Arc`。

「不推进指纹」是「修好即生效」的关键：坏文件期间每次 `start()` 都会重试，用户改好文件的
**下一次委派**立刻用上新配置。代价是坏文件期间每次 spawn 多一次 `stat` + 一次读盘（微秒级）。

线程安全：`Mutex<State>` 保证多 supervisor 并发 `start()`（`yi-agent-store/src/runtime.rs:1834`
的 `WorkspaceAssignedFactory` 会把 `start()` 转发给内层 factory）下不撕裂。

### 4.3 daemon 侧：factory 按指纹重建派生件

`DaemonAgentWorkerFactory::new(...)` 的签名与语义**逐字节不变**（22 处测试构造点不受影响，
见 §1.2 的统计），按 `RuntimeBinding::fixed` / `managed` 的同一模式新增一个 builder：

```rust
struct WorkerRefresh {
    cfg: RuntimeConfig,          // resolve_effective 之前的基准 cfg
    watcher: Arc<CatalogWatcher>,
    cache: Mutex<Option<(EffectiveKey, Arc<WorkerParts>)>>,
}
struct WorkerParts {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    config: AgentConfig,
    sandbox: SandboxMode,
    sandbox_writable_roots: Vec<PathBuf>,
}
```

`with_catalog_watcher(cfg, watcher)` 装上它。`start()` 内（仅当装了）：

```
let catalog = self.refresh.watcher.current();
let effective = resolve_effective(&refresh.cfg, &catalog, catalog.subagent_model.as_deref());
let key = EffectiveKey::of(&effective);          // 显式字段比较
if cache miss { 重建 WorkerParts; 存回 cache }
用 parts 起 worker
```

`EffectiveKey` 取**显式字段**且相等判断：`provider` / `api_url` / `model` / `api_style` /
`reasoning_effort` / `api_key` 全等才算 hit。刻意**不派生 `Debug`、不写日志、不落盘**——
`api_key` 是敏感字段，不得出现在任何可打印/可持久化的位置。

未装 refresh 的 factory：继续用 `new()` 传入的静态字段，**行为逐字节不变**（零回归硬约束）。

装配点唯一：`attach.rs:137` 的 `build_worker_factory` 改为装 refresh（它已握有 `cfg`；
`models_path` 需从 `worker_factory` 一路传下来，该参数本就存在）。

### 4.4 app-server 侧：`production_factory` 换用 watcher

签名从 `catalog: Arc<ModelCatalog>` 改为 `catalog: Arc<CatalogWatcher>`，闭包内一行：

```rust
let catalog = catalog.current();      // 每次建 agent 前取当前清单
let mut thread_cfg = resolve_effective(&thread_cfg, &catalog, model_ref.as_deref());
```

闭包本就在每次调用时重建 provider，**不需要再加缓存**，改动极小。调用点分两类：

- **生产**：`server.rs:1421` 与 `server.rs:1494` 各自的 `load_catalog()` → `CatalogWatcher::new(...)`；
  `ws.rs:59` 同样。三者随后把 watcher 传给 `production_factory`（`server.rs:1449` / `server.rs:1523` / `ws.rs:60`）。
- **测试**：`server.rs` 内 10 处 `production_factory(cfg, test_theme(), test_catalog()…)` 改用
  `CatalogWatcher::from_catalog(...)` 造固定快照，既有测试（如
  `thread_start_and_resume_report_the_built_agents_model`，`server.rs:20036`）语义不变。

副作用是好的：`server.rs:3521` 那条「工厂快照 vs 磁盘重读会分歧」的注释与顾虑随之消失
（工厂不再持快照），上报口径与「实际运行」天然一致。

### 4.5 数据流

```
        ┌──────────────── 进程启动 ────────────────┐
        │  CatalogWatcher::new(models_path)        │  ← 读一次 + 记指纹
        └──────────────────┬───────────────────────┘
                           │
   每次 start()/建 agent ──┤
                           ▼
                 watcher.current()
                           │
        stat(models.json) ─┴─ 指纹相同 ──► 返回缓存 Arc（零解析）
                           │
                        指纹不同
                           ▼
              try_load_catalog_from(path)
                 │                    │
              Ok(catalog)          Err(reason)
                 │                    │
      换缓存 + 推进指纹          warn! + 保留旧值 + 不推进指纹
                 │                    │
                 └────────┬───────────┘
                          ▼
              resolve_effective(cfg, catalog, subagent_model)
                          │
         daemon: EffectiveKey 命中缓存 ──► 复用派生件
                 未命中 ─────────────► 重建 provider/agent_config/...
         app-server: 直接 bootstrap_agent（每次都新建 provider）
```

## 5. 错误处理与边界

| 情况 | 表现 |
|---|---|
| `models.json` 被手工改成坏 JSON | 保留旧清单 + trace warn；委派照常跑（可能仍失败于旧配置，但不因坏文件新增失败） |
| 坏文件随后改好 | **下一次委派生效**，无需重启（§4.2「不推进指纹」保证） |
| `models.json` 被删除 | 采纳空清单 → 回退 `cfg`（明确意图，非错误） |
| 清单合法但引用了不存在的 `subagent_model` | 走既有语义：`effective_entry` 回退全局默认，再回退 `cfg`（不变） |
| 新条目 provider 名非法 | 重建派生件时 `build_provider` 报错 → 记 trace + 保留旧派生件（last-known-good），委派不因一次坏编辑被打断 |
| 并发多个 `start()` | `Mutex` 串行化状态变更；指纹相同时共享同一 `Arc`，无重复解析 |
| 文件系统 mtime 粒度粗（同秒两次写入、size 恰好相同） | 已知取舍：mtime+size 判据与仓库既有做法一致；原子替换写入必定改变二者之一 |

## 6. 明确取舍

### 6.1 只覆盖 `models.json`，不覆盖 `.env`

`.env` 是兜底层，`2026-10-09-model-catalog-authority-design.md` §5 已约定「显式接管」：
导入清单后 `.env` 的改动**不该**再影响生效值。此外 `cfg` 还承载 provider/model/max_turns/
sandbox 等启动期参数，让它在运行期被 `.env` 穿透会大幅扩大影响面。故本次只盯清单。

### 6.2 `provider_profile_id()` 保持 `"daemon-default"`

`lib.rs:475` 返回固定串 `"daemon-default"`，被 `yi-agent-store/src/runtime.rs:505` 用作
daemon 级 LLM 准入的分组标识。它**不参与实际请求**，改成配置派生值会改动准入行为
（带来零收益的回归风险）。重读后它描述的是「该 daemon」（其配置可变），语义仍成立。

### 6.3 指纹用 (mtime, size)，不用内容哈希

每次 `start()` 一次 `stat`（微秒级）远低于每次读盘+哈希。取舍：无法捕获「mtime 被刻意
保持不变且 size 不变」的写入——这种写入在正常编辑/原子替换中不出现。

## 7. 测试策略

### 7.1 `yi-agent-runtime`（`models.rs`）

- `try_load_catalog_from`：文件不存在 → `Ok(空)`；合法 → `Ok`；坏 JSON → `Err`；
  不可读 → `Err`（不把损坏当空）。
- `CatalogWatcher`：
  - 指纹未变 → `current()` 返回同一 `Arc`（用 `Arc::ptr_eq` 断言，证明零重读）；
  - 改动文件 → 返回新清单；
  - 坏文件 → 保留旧清单（`ptr_eq` 旧值）+ 不推进指纹；
  - 坏文件改好（下次 `current()`）→ 立即采用新值（**本修复的核心断言**）；
  - 文件删除 → 采纳空清单；
  - `from_catalog` → 永不重读（等价固定快照）。

### 7.2 `yi-agent-subagent`（`attach.rs` / `lib.rs`）

- 装 refresh 后：改 `models.json` 的 `api_url` 与 `subagent_model` → **下一次 `start()`**
  的解析结果用新值（用 `worker_config_model` 之类的可观测接缝断言模型名切换）。
- 坏文件 → 沿用旧配置，不 panic、不阻断。
- 坏文件改好 → 生效。
- **未装 refresh 的 factory：行为逐字节不变**（既有 10 处测试构造点必须全绿，是零回归门禁）。

### 7.3 `yi-agent-app-server`

- `production_factory` 换 watcher 后：改 `models.json` 新增条目 → **新建 thread**
  （`thread/start`）即命中新条目模型，无需重启 app-server。沿用
  `thread_start_and_resume_report_the_built_agents_model`（`server.rs:20036`）的 harness 风格。
- 既有线程级模型测试（`thread/setModel` 等）全部保持绿。

### 7.4 真机端到端（决定性判据）

隔离环境（临时 workdir + `YI_AGENT_RUNTIME_DIR`）起一个真实 daemon，全程**不重启它**：

1. 清单为可用值 → 委派成功（`state: completed*`）；
2. 改清单 `api_url` 为坏值 → 委派失败（证明**确实重读了**，不是缓存侥幸）；
3. 改回可用值 → 委派再次成功（证明**修好即生效**）。

对照本次事故：旧 daemon 在「配置已修好」后仍是 112→120 次 404 / 0 次成功；修复后
第 3 步必须直接成功。

### 7.5 回归门禁

- `cargo test -p yi-agent-runtime --lib`、`-p yi-agent-subagent`、`-p yi-agent-app-server --lib`、
  `-p yi-agent`。
- `cargo fmt --all` + workspace `clippy -D warnings`。
- 既有 `attach.rs` 的 10 处 `DaemonAgentWorkerFactory::new` 测试点不改一行（证明签名未动）。

## 8. 影响面清单

| 文件 | 改动 |
|---|---|
| `yi-agent-rs/crates/yi-agent-runtime/src/models.rs` | 新增 `try_load_catalog_from`、`CatalogWatcher`（+ 单测） |
| `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs` | `build_worker_factory` 装 `with_catalog_watcher`；`models_path` 下传 |
| `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` | `DaemonAgentWorkerFactory` 新增 `WorkerRefresh`/`WorkerParts`/`EffectiveKey`、`with_catalog_watcher`、`start()` 内取当前清单 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | `production_factory` 收 `Arc<CatalogWatcher>`；2 处生产调用点（`:1421`/`:1494`）+ 10 处测试调用点改 `from_catalog` |
| `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs` | 同上（`:59` 的 `load_catalog()` → watcher，`:60` 传入） |
| `docs/bug-list.md` | 登记本条修复（根因、判据、验证） |
| `docs/project-management/*` | 按 CLAUDE.md 同步模块进度 |

## 9. 未决 / 风险

- **mtime+size 判据**的固有盲区见 §6.3；若日后需要更强保证，可换内容哈希（接口不变，
  只改 `Fingerprint` 与 `current()` 内部）。
- **app-server 侧仍有别的启动快照**（如 `attach_cwd_runtime` 的 `cfg`）不受本次影响：
  它们承载的是启动期参数而非清单条目，不在本问题范围内。
- 本次不动 `models.json` 的写入路径与 UI 提示；「地址缺 `/v1`」的告警仍由既有设计承担。