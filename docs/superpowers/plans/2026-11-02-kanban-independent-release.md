# Superpowers 看板独立发布 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在不更新 yi-agent 应用包的情况下安装、更新和卸载包含界面、执行程序及 skill 的官方看板插件，并完整满足设计 A01–A20。

**Architecture:** 主程序只提供通用插件宿主；独立 Web UI 通过隔离容器和版本化桥访问受管插件。包验证与生命周期核心放在不依赖 app-server/store 的新 Rust crate，桌面、CLI、daemon 通过同一实例注册表、跨进程锁及持久化事务协调；业务仍归看板独立 workspace。

**Tech Stack:** Rust、Tauri 2/WebView、React/TypeScript/Vite、serde JSON、Unix socket/flock、Vitest、现有 cargo nextest/just 门禁。签名和归档采用经审阅的成熟库；不自造密码算法，不使用有已知漏洞的解包路径。

## Global Constraints

- 依据：[设计 spec](../specs/2026-11-02-kanban-independent-release-design.md)。本文为完整实施路线，不表示任务已经完成或新 API 已存在。
- “首版仅支持官方可信插件。”
- “界面、执行程序、skill 不进入主程序发布物。”
- “安装与项目启用分开；后台常驻与模型任务执行单独授权”。
- “页面隔离不代表原生程序沙箱。”
- “首版每实例一个激活版本，所有启用项目一起协调。”
- “程序安装及实例私有状态必须隔离，但本次不将既有整机限流改成实例各自一份。”
- “旧会话不热替换 skill”。
- “不提供任意 RPC、任意文件访问或 shell 执行后门。”
- “首版发布必须覆盖完整目标，不以界面暂时内置或仅本地可用替代。”
- 所有变更先在 `.worktrees/` 的非 main 分支完成；本计划的编写不授权开始执行、发布或使用生产签名密钥。
- 技术验证 G0 不通过不得继续产品容器实现；子协议 G1/G2/G3 未审阅不得以本计划中的接口草案冒充已批准线协议。
- 不调用真实 LLM；driver/会话验收使用可控 mock。原生 WebView、OS 下载运行和远程连接必须真实验证。

---

## 0. 阅读顺序、工作目录与门禁

先读 spec，再读本节、接口总表和当前任务。仓库根为 `R`，Rust 主 workspace 为 `R/yi-agent-rs`，插件 workspace 为 `R/plugins/superpowers-kanban`，桌面前端为 `R/desktop`。

以下示例均使用任务 worktree，不从主仓库借用运行目录或共用 `CARGO_TARGET_DIR`；不要使用真实项目、默认 `~/.yi-agent` 或生产 relay 做破坏性测试。开始时在 worktree 根执行 `export R="$PWD"`；每一条独立命令默认从 `cd "$R"` 开始，括号里的 `cd` 仅影响该子 shell。Rust 核心测试用绝对 manifest 路径，避免不同 workspace 混淆。Files 中完整仓库相对路径优先；紧随某 crate/目录之后的 `src/...`、`tests/...`、文件 basename 继承该明确目录，不是仓库根的同名文件。

本文是带审阅门槛的总计划，不将尚未验证的容器/线协议伪装成可直接照抄的完整代码。T01–T05 可按本文准备验证与子协议；后续任务是独立审阅单元，不是一次 2–5 分钟动作。G1/G2/G3 批准时分别产出对应的执行子计划（T03/T04/T05 指定路径），将每个功能切成单一行为的“测试代码 → 行为红 → 最小实现代码 → 同命令绿 → 提交”小循环；本表的多个负例不得合成一个巨型实现步骤。子计划必须逐项继承本文文件、接口、前置、错误分支及 C 门禁，重新核对基线行号，并用 writing-plans 检查代码/命令完整性后才允许执行。协议变更必须先同步本总计划和下游子计划，不授权工程师临时选择关键产品决策。

### 历史基线阻塞 B0 与执行前复验

编写计划时曾复现：`zip 8.6.0` 命中 `RUSTSEC-2026-0429`，`just audit` 失败。后续 main 已通过独立维护变更升级到 zip 9.2.2（合并 `27ed7276`），本分支同步纳入该修复，不在本计划中偷加审计豁免或重复升级依赖。B0 的旧漏洞已有修复，提交/合并和后续实现仍必须在当前基线上重新跑完整门禁；出现新的失败先处理，不沿用旧的绿色结果。

```bash
cd yi-agent-rs
cargo fmt --all -- --check
just ci
```

预期：全部门禁 exit 0。失败时记录具体步骤，先处理基线，不把后续失败归咎于本任务。

### 每个代码任务的统一完成门禁 C

1. 运行该任务明确列出的失败测试，确认真实执行且至少一个目标断言失败；编译失败只证明接口未接入，接入后还要做一次行为失败验证。不得接受零测试、跳过或误过滤的绿色输出。
2. 实现后运行同一命令通过，并验证相关错误分支；必要时短暂移除保护条件，确认对应负例失败，再恢复代码。
3. 按触及的 workspace 做格式/静态检查：主 Rust `cargo fmt --all`，插件 workspace `cargo fmt --all`，前端 `npm ci && npm run build && npm test`；正式 WebView 另跑 E2E，不用单测代替。
4. 在主 Rust workspace 运行 `just ci` 全绿；独立插件与前端测试也必须分别通过。审阅 `git diff --check` 和改动范围。
5. 同一 commit 更新对应 `docs/project-management/` 模块及索引计数。只将测试与实现均完成的项目标 `[x]`，不能按“文件已创建”标完成。
6. 仅显式 `git add` 当前任务文件和文档，然后用任务给出的 conventional commit 标题提交。合并遵循仓库流程，在合并结果上再次 `just ci`；未到集成阶段不自动合并或删除审阅中的 worktree。

命令产生的测试日志、证据报告、临时根、包和密钥写入被忽略的任务产物目录，不提交真实凭据。失败报告要保留错误，不将非零 exit、unsupported 或 timeout 转成成功。

## 1. 文件与模块归属

| 目录/文件 | 职责 | 禁止依赖 |
|---|---|---|
| `plugins/contracts/` | JSON schema、黄金协议样本、测试发布样本描述 | 宿主私有 React 类型 |
| `yi-agent-rs/crates/yi-agent-plugins/`（新） | 包/信任/注册/授权/准入/事务/受管解析核心 | app-server、Tauri、store/sqlite、看板 crate |
| `yi-agent-rs/crates/yi-agent-app-server/src/plugin_manager/`（新） | 网络获取、管理 RPC、运行协调、会话/driver 适配 | 看板业务类型 |
| `yi-agent-rs/crates/yi-agent-supervisors/src/managed.rs`（新） | 受管启动、维护暂停、项目所有权 | 靠插件名选择业务 |
| `desktop/src-tauri/src/plugin_host/`（新） | 已验证资源、缓存、容器及原生侧恢复 | 任意资源路径/任意原生桥 |
| `desktop/src/lib/pluginHost/`（新） | 宿主桥、绑定、事件及视图描述 | 看板卡片状态 |
| `plugins/sdk/`（新） | 插件前端最小桥客户端，独立构建 | 宿主业务组件/store |
| `plugins/superpowers-kanban/ui/`（新） | 看板界面、设置及其测试 | `desktop/src/` 源码导入 |
| `plugins/superpowers-kanban/crates/` | 看板后端及工作流适配 | 链接主程序 Rust 业务 crate |
| `plugins/fixtures/example-counter/`（新） | 第二插件和安全/故障验收输入 | 进入正式主程序发布物 |
| `scripts/plugins/`（新） | 打包、正式 E2E 编排、报告校验、发布物检查 | 生产凭据硬编码 |

新增 crate 时必须创建 `docs/project-management/yi-agent-plugins.md` 并登记索引；不把整个插件管理平台塞进原有大 `server.rs` 或 `bridge.rs`。

## 2. 任务依赖与审阅门槛

```text
B0 ───────────────────────────────────────────────────────────┐
T01 → T02 → G0 → T03(G1) → T04(G2) → T05(G3)                   │
                         ↓                                     │
T06 → T07 → T08 → T09 → T10 → T11 → T12 → T13 → T14 → T15     │
                                      ↓                        │
T16 → T17 → T18 → T19 → T20 → T21 → T22 → T23 → T24 → T25     │
                                                            ↓  │
                                                         T26 → T27
```

- 图中省略的显式前置见各任务；编号不是并行授权。默认串行执行。
- G0：真实 WebView 技术报告通过，或容器变更重新审阅通过。
- G1：包、两端 API、能力桥、受管命令、项目/代次子 spec 审阅通过。
- G2：发布信任、状态机、RPC、共享配额、恢复子 spec 审阅通过。
- G3：看板迁移、工作流依赖、旧数据和远程资源子 spec 审阅通过。
- B0 是所有代码提交的前提；G1–G3 是后续产品实现的前提。技术探针仅在 B0 解除后提交，不借探针绕过尚未批准的产品协议。

## 3. 接口草案与一致性规则

以下名字是计划中的统一接口草案，不是仓库现有 API。T03–T05 将语义、序列化和权限固定进子 spec；审阅如改名，必须同时更新本表、所有测试和后续任务，再开始实现。

### 核心值与错误

`yi-agent-plugins/src/types.rs` 定义下列独立、可序列化值类型，标识内部字符串须通过校验构造：

```rust
pub struct PluginId(String);
pub struct ProjectId(String);
pub struct ReleaseId {
    pub plugin: PluginId,
    pub version: String,
    pub target: String,
    pub sha256: String,
}
pub struct Binding {
    pub host_id: String,
    pub release: ReleaseId,
    pub activation: u64,
    pub project: ProjectId,
    pub grant: u64,
}
pub enum Effect { Read, Write, Launch, QuotaWrite, Resident, Maintenance }
pub struct Operation {
    pub request_id: String,
    pub binding: Binding,
    pub effect: Effect,
    pub parent_turn: Option<String>,
}
```

`PluginError` 至少区分 `InvalidManifest`、`Integrity`、`Untrusted`、`Expired`、`Revoked`、`Incompatible`、`NotSupported`、`PermissionDenied`、`StaleBinding`、`Dependency`、`Conflict`、`Draining`、`Busy`、`ResultUnknown`、`RecoveryRequired`、`Io`。JSON 错误包含稳定 `kind`，不将任意 Rust debug 字符串当线协议。

### 核心模块公共接口

```rust
// manifest.rs
Manifest::parse(bytes: &[u8]) -> Result<Manifest, PluginError>;
// trust.rs；VerifiedRelease 不可由未经验证 JSON 直接构造。
TrustStore::bootstrap(root_envelope: &[u8]) -> Result<TrustStore, PluginError>;
verify_metadata(envelope: &[u8], trust: &mut TrustStore, now: u64)
    -> Result<VerifiedRelease, PluginError>;
// bundle.rs
install_verified(release: &VerifiedRelease, archive: &Path,
    destination: &Path, limits: &ExtractLimits) -> Result<InstalledRelease, PluginError>;
// registry.rs；写操作内部使用短期跨进程锁。
Registry::open(instance_home: &Path) -> Result<Registry, PluginError>;
Registry::record_install(&mut self, release: &InstalledRelease) -> Result<(), PluginError>;
// grants.rs
Registry::grant(&mut self, plugin: &PluginId, project: &ProjectId,
    capabilities: &[Effect]) -> Result<Binding, PluginError>;
Registry::check(&self, binding: &Binding, effect: Effect) -> Result<(), PluginError>;
// admission.rs
Admission::open(instance_home: &Path) -> Result<Admission, PluginError>;
Admission::begin_turn(&self, binding: &Binding, turn_id: &str) -> Result<TurnPermit, PluginError>;
Admission::begin(&self, operation: &Operation) -> Result<OperationPermit, PluginError>;
Admission::drain(&self, plugin: &PluginId) -> Result<DrainTicket, PluginError>;
// lifecycle.rs；共享同一实例的 registry/admission/journal 路径。
Lifecycle::open(instance_home: &Path) -> Result<Lifecycle, PluginError>;
Lifecycle::start(&self, request: LifecycleRequest) -> Result<TransactionId, PluginError>;
Lifecycle::advance(&self, id: &TransactionId, runtime: &dyn RuntimeControl)
    -> Result<Progress, PluginError>;
Lifecycle::recover(&self, runtime: &dyn RuntimeControl) -> Result<Vec<Progress>, PluginError>;
```

`InstalledRelease` 只包含受管根、验证身份与文件摘要，不能把调用方任意路径伪装成安装结果。`TurnPermit` / `OperationPermit` 持有真实存活锁；进程退出释放，不能仅按 TTL 或 PID 判断已无操作。`DrainTicket` 可查询/取消，取消恢复旧准入；不通过 drop 自动猜测回退是否安全。

`LifecycleRequest` 只含 `Activate`、`DisableProject`、`Uninstall`、`Rollback` 四种明确动作；包下载不借用 `Activate` 隐式启用项目。`Progress` 含事务 ID、phase、blocking projects/turns、错误与是否可恢复。`RuntimeControl` 只做维护暂停/恢复、停止/启动候选服务、健康检查和项目所有权协调，不能调用看板业务迁移以外的任意脚本。

T03/T04 必须把这里的语义类型展开成 Rust 字段与线协议：`PluginRecord`、`LifecycleRequest`、`TransactionId`、`Progress`、`RuntimeControl` 每个方法、`OperationOutcome`、权限变更请求和结果查询。`Registry::grant` 是宿主内部事务原语，不允许插件直接调用，也不单独代表启用完成；首次启用先在维护事务中建立初次激活代次，再启动/健康检查并提交授权，失败不留下可用 binding。后续权限变更先排空受影响项目再递增 grant；`Maintenance` 只可由生命周期事务签发，不在用户能力选择或插件 method 可申请列表中。只读授权不阻止当前 OS 用户权限的 native 文件访问，UI 必须保留原生信任提示。

### TS 桥与管理 RPC 草案

```ts
export type PluginHost = {
  context(): Promise<PluginContext>;
  request<T>(method: string, params: unknown, requestId: string): Promise<T>;
  result<T>(requestId: string): Promise<OperationResult<T>>;
  subscribe(topic: string, after: number, onEvent: (e: PluginEvent) => void): () => void;
  openThread(threadId: string): Promise<void>;
  notify(text: string): Promise<void>;
  confirm(text: string): Promise<boolean>;
};
```

`PluginContext` 提供已授权项目句柄、语言/主题令牌、能力和完整 binding 的非秘密身份字段；不暴露原生认证令牌。`OperationResult<T>` 为 `pending | committed | unknown | rejected` 的判别联合；`PluginEvent` 包含 `topic`、`revision`、`binding`、`payload`。

管理方法使用 `plugins/v1/catalog`、`list`、`install`、`enableProject`、`changeGrant`、`disableProject`、`activate`、`rollback`、`transaction`、`cancelTransaction`、`uninstall`、`clearData`、`gc`、`inspectLegacy`、`adoptLegacy`、`assetRelease`、`operationResult`（均带 `plugins/v1/` 前缀）；业务桥仍只请求自己的已授权后端。下载/安装与启用方法分开；旧 `plugin/install` 不得成为新授权旁路。`clearData` 独立于默认卸载，需宿主列明范围并二次确认；`gc` 只清引用安全的受管产物。配额/常驻配置通过具名且受授权的宿主能力，不能借任意业务 method 修改实例管理文件。G1 固定这些能力的 method 与 schema，T16 接入管理核心，T23 接入 inspect/adopt，T24 完成 clear/gc；尚未实现的方法返回稳定 NotSupported，不伪装成功。

受管 CLI 草案：`yi-agent plugins exec --plugin example-counter --project-handle project-1 --binding-file session-plugin-binding.json -- increment`。例中的 binding 文件由宿主生成并绑定当前会话/宿主，不能接受页面上传文件作授权。实际 skill 使用宿主提供的绝对启动入口及文件路径，不要求用户 PATH 有 `yi-agent` 或插件程序。

headless 管理草案为 `yi-agent plugins catalog|list|install|enable-project|change-grant|disable-project|activate|rollback|transaction|cancel-transaction|uninstall|clear-data|gc|inspect-legacy|adopt-legacy`。`install` 二选一接收官方目录 release 身份或 `--bundle` 加 `--metadata` 的本地官方签名输入；两者均走同一验证流程。命令选择的实例遵循 `YI_AGENT_HOME`，项目显式选择，管理命令不得携带页面业务权限。T03 固定 argv/JSON 输出与版本握手，T14 实现 exec，T16 接入管理子命令与同源应用服务（不是要求 headless 用户先启动桌面）。

## 4. 测试夹具约定

T03 创建 `plugins/contracts/fixtures/` 下：`root-v1.json`、`release-v1.json`、`manifest-v1.json`、`bridge-v1.json`、`lifecycle-v1.json`。全部是可公开的测试身份；生产构建不嵌入测试信任根。

T06 创建 `yi-agent-plugins/tests/common/mod.rs`，其最小公共测试 API：

```rust
pub struct Fixture {
    pub tmp: tempfile::TempDir,
    pub root: std::path::PathBuf,
    pub bundle: std::path::PathBuf,
    pub envelope: Vec<u8>,
    pub trust_root: Vec<u8>,
}
pub fn signed_fixture() -> Fixture;
pub fn fixture_bytes(name: &str) -> Vec<u8>;
```

`fixture_bytes` 从 `CARGO_MANIFEST_DIR/../../../plugins/contracts/fixtures` 读指定文件，失败即 panic，不返回空默认值。`signed_fixture` 在临时目录用公开固定测试种子、黄金 schema 和无害 counter 程序生成完整 tar.gz/元数据；不得调用尚未实现的生产打包工具。所有 bundle、路径、journal 断言都指向 `Fixture.tmp`，不接触真实 home。

WebView E2E 输出 JSON 报告，字段至少为 `suite`、`appSha256`、`containerConfigSha256`、`cases[] { id, status, evidence }`。测试脚本必须核对预期 case 集合，`skipped`、`unsupported`、缺 case、空报告、timeout 或非零进程退出都失败。证据只允许日志和测试截图路径，不含秘密。

---

## Phase 0：先证明容器可行

### Task T01：建立隔离 WebView 技术探针和可执行报告

**前置：** B0。

**Files:** Create `desktop/src-tauri/src/plugin_host/mod.rs`、`probe.rs`、`desktop/scripts/plugins/probe.mjs`、`probe-lib.mjs`、`plugins/fixtures/example-counter/probe/index.html`、`probe/attacks.js`、`desktop/scripts/plugins/probe-report.test.mjs`。Modify `.gitignore`（忽略 `/.artifacts/`）、`desktop/src-tauri/src/lib.rs`、`desktop/src-tauri/Cargo.toml`、`desktop/src-tauri/tauri.conf.json`、`desktop/package.json`。探针代码只通过非默认 `plugin-probe` feature 接入，不进入正式包。

**Interfaces:** 输入为隔离实例根、probe suite、输出报告路径；输出真实 WebView 运行报告。探针与后续产品共用容器创建和资源策略，不能在测试模式关闭安全配置。测试身份只在探针构建中注入，不改变正式信任根。`probe-lib.mjs` 导出纯函数 `checkProbeReport(report)`，导入不构建、不启动进程；`probe.mjs` 仅作 CLI 编排。原生 `probe.rs` 通过仅探针 feature 可用的宿主控制通道接收 case 回报，并独立核对 frame、case、nonce 与超时；插件不能自行报告“已恢复”。恢复 case 的成功需由原生侧关闭/重建后，用正常宿主交互及持久化会话校验确认。

- [x] Step 1：用 Node 内置测试写报告验收，先确认空 case 会失败：
  ```js
  import test from 'node:test';
  import assert from 'node:assert/strict';
  import { checkProbeReport } from './probe-lib.mjs';
  test('empty report is not a pass', () => {
    assert.throws(() => checkProbeReport({cases: []}), /missing/);
  });
  ```
- [x] Step 2：运行 `node --test desktop/scripts/plugins/probe-report.test.mjs`，确认缺少实现/校验时红；新增错误报告 fixture，确认不是只因 import 错误红。
- [x] Step 3：实现 probe 构建入口、独立临时 `YI_AGENT_HOME`、本机假后端、资源加载与消息记录；攻击用例包括 DOM/storage、invoke、跨包资源、导航/弹窗/图片外送、frame 冒领、重放初始化、持续阻塞和有界大量分配。原生侧设截止时间、关闭/重建容器；外部 runner 另设整组兜底超时，超时终止整个测试进程组并记 FAIL，不能把“杀掉应用”算恢复成功。禁止无界耗尽机器内存。
- [x] Step 4：从仓库根执行下列构建，再运行 `node desktop/scripts/plugins/probe.mjs --suite isolation --output .artifacts/plugins/probe.json`；runner 用 `cargo metadata --manifest-path desktop/src-tauri/Cargo.toml --no-deps --format-version 1` 解析 target_directory，核对 bundle/二进制存在及摘要，不猜主 workspace target。不用 `open -a`，macOS 直接运行 bundle 的 `Contents/MacOS/desktop`（当前 package 名为 desktop）；其他目标平台必须有对应启动适配器，否则不列入支持矩阵。
  ```bash
  (cd desktop && npm ci && npm run sidecar:release && npm run build && npm run tauri -- build --bundles app --features plugin-probe)
  node --test desktop/scripts/plugins/probe-report.test.mjs
  ```
  sidecar 必须由当前 worktree 构建且使用隔离实例；报告原生写入测试临时根，runner 等待原子报告及明确正常退出，收集 stderr 与摘要，finally 先停止桌面父进程再清理其后代。不借正在使用的正式 sidecar。
- [x] Step 5：按 C 验证并提交 `test: add isolated plugin webview probe`。管理文档记录探针命令、正式配置摘要与支持平台，不标插件已完成。

### Task T02：审阅技术结果并解除 G0

**前置：** T01。

**Files:** Create `docs/superpowers/specs/2026-11-02-plugin-container-validation.md`。Modify T01 容器文件和探针负例，仅在失败定位后修改。

**Interfaces:** 消费真实报告；输出逐条对应 spec §4 的 PASS/FAIL 证据、选定资源机制、sandbox/CSP/原生权限配置及恢复策略。

- [x] Step 1：报告验证脚本增加断言：case 必须包含 `blocked_script_recovery`、`native_invoke_denied` 和 `cross_frame_replay_denied`，删除任一 case 后测试必须红。
- [x] Step 2：用同一 release 容器配置重跑正反例，记录配置差异；无主页面 JS 响应时验证原生侧仍能恢复容器与核心交互，并校验会话持久化不受损。正常用例逐项验证中文 IME、键盘/焦点、滚动、主题和尺寸，报告不得只覆盖攻击用例。
- [x] Step 3：若 iframe 在所支持 WebView 上无法恢复，停止后续实现；提交容器替代设计，用户批准后重跑 G0。不能把测试改成“跳过该平台”。
- [x] Step 4：审阅完整报告和目标平台矩阵，显式取得 G0 认可。没有报告或许可不进入 T03 之后的产品容器实现。**G0 已由用户于 2026-11-03 明确批准**（三处取舍一并确认：接受 `script-src 'unsafe-inline'`、首版矩阵仅 macOS arm64、接受 `close_and_rebuild` 丢弃页面未持久化状态）。

  > 复验得到的阻断项 B1（宿主自定义命令绕过 ACL）已按方案 1 修复：`build.rs` 声明 `APP_COMMANDS` 生成 app ACL manifest，`capabilities/default.json` 为**主窗口**逐条放行 12 条命令，探针改注册生产命令集，攻击用例只在 `not allowed by ACL` 时才算通过，并新增 `desktop/src-tauri/tests/app_command_capability.rs` 把三份清单绑在一起。修复后实测 `rpc` / `set_relay_url` / `restart_sidecar` 均返回 `not allowed by ACL`。本步仍未勾选：G0 只能由用户批准。
- [x] Step 5：按 C 提交 `docs: record plugin container feasibility gate`。文档提交也不得绕过现有仓库提交门禁。

## Phase 1：先冻结跨模块协议

### Task T03：完成宿主协议子 spec，解除 G1

**前置：** G0。

**Files:** Create `docs/superpowers/specs/2026-11-02-plugin-host-protocol.md`、执行子计划 `docs/superpowers/plans/2026-11-02-plugin-host-runtime.md`（T06、T10、T11、T14、T15、T17–T20）、`plugins/contracts/manifest-v1.schema.json`、`host-v1.schema.json`、`bridge-v1.schema.json`、`plugins/contracts/package.json`、`package-lock.json`、`plugins/contracts/fixtures/root-v1.json`、`release-v1.json`、`manifest-v1.json`、`bridge-v1.json`、`scripts/plugins/check-contracts.mjs`、`check-contracts.test.mjs`。

**Interfaces:** 冻结 §3 值类型、错误、RPC、CLI、桥；发布清单声明业务 method/CLI verb 的 effect 和能力，未知方法不允许由调用方自报为 Read。声明宿主保留环境变量和支持 API 范围。

- [x] Step 1：给 schema checker 写失败测试：合法 golden 可通过，未知必需字段、非法入口、错误 binding、任意 RPC 和重复请求 ID 参数冲突必须拒绝。（`scripts/plugins/check-contracts.test.mjs`，21 条断言，先红后绿）
- [x] Step 2：`node --test scripts/plugins/check-contracts.test.mjs` 验证行为红，再补 schema/checker。依赖固定在 `plugins/contracts/package.json`（ajv 8.17.1 / ajv-formats 3.0.1）与 lock，脚本用 `createRequire` 指向该 package.json 解析依赖。schema 只测报文结构；重复 ID 参数冲突和生命周期顺序用 checker 中显式 trace validator 验证。
- [x] Step 3：在子 spec 固定版本握手、项目句柄、双代次、消息顺序、原始签名 payload 的编码规则、错误恢复及权限矩阵，同步 §3 草案不留两套名字；固定默认安全限制：包 256 MiB、解压总量 1 GiB、单文件 128 MiB、文件数 20,000。见 `docs/superpowers/specs/2026-11-02-plugin-host-protocol.md`。**待 G1 审阅**：该 spec §14 列出 4 项需审阅者决定的事。
- [x] Step 4：运行 `npm ci --prefix plugins/contracts && node scripts/plugins/check-contracts.mjs && node --test scripts/plugins/check-contracts.test.mjs`；检查正反样本实际计数，审阅并获得 G1。（实测：`positive: 4, negative: 17, failures: 0`、测试 21/21。**G1 已由用户于 2026-11-03 批准**，四项待决事项均按建议通过：①保持 `namespace/verb` 加 T05 映射表；②破坏性动词表入协议；③句柄撤销粒度=项目，不建签发表；④未知能力一律视为必需。）
- [x] Step 5：按 C 提交 `docs: define versioned plugin host contracts`（`d2a39de4`）。

### Task T04：完成分发/生命周期子 spec，解除 G2

**前置：** G1。

**Files:** Create `docs/superpowers/specs/2026-11-02-plugin-distribution-lifecycle.md`、执行子计划 `docs/superpowers/plans/2026-11-02-plugin-distribution-lifecycle.md`（T07–T09、T12、T13、T16、T24、T26、T27）、`plugins/contracts/root-v1.schema.json`、`release-v1.schema.json`、`lifecycle-v1.schema.json`、`plugins/contracts/fixtures/lifecycle-v1.json`。Modify `scripts/plugins/check-contracts.mjs`、`check-contracts.test.mjs` 及 T03 的 root/release 样本（此时才固定密码学有效字节）。

**Interfaces:** 消费 G1；冻结 `LifecycleRequest`、`Progress`、`RuntimeControl`、事务阶段、锁顺序、崩溃恢复表和 trust store 规则。

- [x] Step 1：加入失败样本：未备份即迁移、候选服务公共可见、部分项目恢复准入、旧 turn 收尾被拒绝、无新授权扩权、共享配置修订被覆盖。
  > 2026-11-03：`check-contracts.mjs` 负样本 17 → 24；lifecycle 新增 4 条（journal 指向别的插件、回滚缺上一激活、未知 phase、`done` 仍带 error），root 1 条（公钥非 32 字节），release 2 条（签名覆盖的字段被删、信封多出未知字段）。
- [x] Step 2：运行 contract tests 证实这些样本红；逐个完成状态表，不通过删样本过关。
  > 2026-11-03：`positive: 9, negative: 24, failures: 0`；`--test` 42/42。正样本新增 lifecycle、root、release 三个 fixture 与两份现算签名信封。
- [x] Step 3：冻结 Ed25519 签名原始 payload 字节、base64 编码、发布序号/有效期、根轮换/撤销、缓存已接受时间及离线规则；具体库版本经 audit 后锁定。固定锁顺序：插件生命周期锁 → 短期实例 registry 锁；不得持 registry 锁等待进程/网络/项目维护。项目所有权保持在 guardian，跨实例争用非阻塞失败；恢复先于准入。
  > 2026-11-03：payload 编码冻结在 `scripts/plugins/sign-contracts.mjs`（`domain || 0x00 || field*`，字段序见 G2 §2.2），root/release fixture 带真实 Ed25519 签名（公开测试种子，非生产密钥）。`ed25519-dalek` 约束 `>= 2.0.0`（`RUSTSEC-2022-0093` 只影响 `< 2`），精确版本由 T07 在 `cargo audit` 后锁定。锁顺序、guardian 所有权、恢复先于准入见 G2 §5、§6。
- [x] Step 4：列出每一持久化阶段的“崩溃后下一动作、可否回滚、何时恢复准入”；固定后端失败窗口、重启次数/退避上限和熔断恢复条件，审阅 cross-instance 配额兼容和原生签名发布策略，获得 G2。
  > 2026-11-03：材料交付（G2 §6 八阶段表、§7 熔断默认值、§8 配额边界、§9 四项待决）。**G2 批准（2026-11-03）**：熔断默认值、`projecting` 不可回滚、共享配置回退 `RecoveryRequired`、`ed25519-dalek >= 2.0.0` 四项均按建议通过。
- [x] Step 5：按 C 提交 `docs: define plugin trust and lifecycle transactions`。
  > 2026-11-03：commit `48e72e9f`。门禁 C：`just ci` 除 `audit` 外 14 项全绿；`audit` 因联网取 advisory-db 失败（`failed to fetch`），改用 `cargo audit --no-fetch --db ~/.cargo/advisory-db` 离线通过（1402 条公告，仅 1 条已在豁免清单的 yanked 告警）。

### Task T05：完成看板迁移子 spec，解除 G3

**前置：** G1、G2。

**Files:** Create `docs/superpowers/specs/2026-11-02-kanban-plugin-migration.md`、执行子计划 `docs/superpowers/plans/2026-11-02-kanban-plugin-migration.md`（T21–T23、T25）、`plugins/superpowers-kanban/contract/managed-v1.json`。Modify contract checker 及其测试。三个子计划按本表依赖交错执行，不得按文件顺序各自从头跑完，尤其不能跳过前置的通用第二插件验证。

**Interfaces:** 冻结看板所有 CLI/method 到 effect 的映射、工作流依赖、状态目录/共享配额注入、旧安装探测规则、数据格式版本与接管协议。

- [ ] Step 1：从现有 CLI usage、`driver-protocol.json`、skill、UI 操作列清单；测试所有现有动词均有类别，`run`/`manifest` 只能维护使用，`merge-request` 不能作为 Read。
- [ ] Step 2：将 `state --self`、锁释放、队列写入与更新排空路径对齐；测试禁止外部新工作但允许旧回合必要收尾。
- [ ] Step 3：列明复制/移动的每个 UI 文件、保留的数据和旧别名。现有 `sessions.running` 是宿主进程上报，不能把它直接当成跨实例全机计数；冻结共享统计/租约协议与回退口径，避免在分发改造中扩大预算。
- [ ] Step 4：固定上游 skills 的可验证依赖方式：有发布版本的按版本范围，普通文件按所需能力/命令契约检查，不能要求不存在的 skill semver。未知兼容性阻止自动任务但允许查看。获得 G3。
- [ ] Step 5：按 C 提交 `docs: define managed kanban migration contract`。

## Phase 2：可信包与受管状态核心

### Task T06：新增独立核心 crate、类型与测试夹具

**前置：** G1、G2、G3、B0。

**Files:** Create `yi-agent-rs/crates/yi-agent-plugins/Cargo.toml`、`src/lib.rs`、`src/types.rs`、`src/error.rs`、`src/manifest.rs`、`tests/manifest.rs`、`tests/common/mod.rs`、`docs/project-management/yi-agent-plugins.md`。Modify `yi-agent-rs/Cargo.toml`、`Cargo.lock`、`docs/project-management/README.md`。

**Interfaces:** 实现 §3 值类型、`Manifest::parse` 和 §4 fixture API。核心路径只由调用方注入，不依赖 store/Tauri/app-server。新公共类型 derives 与 schema 保持一致。

- [ ] Step 1：增加失败测试：
  ```rust
  #[test]
  fn absolute_ui_entry_is_rejected() {
      let mut v: serde_json::Value = serde_json::from_slice(
          &common::fixture_bytes("manifest-v1.json")).unwrap();
      v["views"][0]["entry"] = "/etc/passwd".into();
      assert!(Manifest::parse(&serde_json::to_vec(&v).unwrap()).is_err());
  }
  ```
- [ ] Step 2：`cd yi-agent-rs && cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test manifest`，先红后绿；同时覆盖非法 ID、重复贡献、未知必需字段和未知 effect。
- [ ] Step 3：创建模块边界及公开测试 fixture builder；仅依赖通用序列化/验证库，按 G2 版本引入签名/归档 dev 依赖以生成夹具。测试私钥只能在 tests 源码，正式库不得嵌入。
- [ ] Step 4：运行测试和 `cargo tree --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins`，确认无 sqlite、app-server、Tauri、看板依赖；新增模块进度条目带对应命令，不预标后续能力完成。
- [ ] Step 5：按 C 提交 `feat: add independent plugin contract core`。

### Task T07：实现发布信任、有效期与撤销状态

**前置：** T06。

**Files:** Create `yi-agent-plugins/src/trust.rs`、`tests/trust.rs`（均在新 crate 下）。Modify `src/lib.rs`、crate 依赖及主 lock。

**Interfaces:** 实现 `TrustStore::bootstrap`、`verify_metadata`、不可伪造 `VerifiedRelease`；trust store 可持久化接受序号、时间下界、根版本和撤销记录。

- [ ] Step 1：写失败测试：
  ```rust
  #[test]
  fn changed_payload_without_new_signature_is_rejected() {
      let f = common::signed_fixture();
      let mut trust = TrustStore::bootstrap(&f.trust_root).unwrap();
      let mut bad = f.envelope.clone();
      let n = bad.len() / 2;
      bad[n] ^= 1;
      assert!(verify_metadata(&bad, &mut trust, 1_800_000_000).is_err());
  }
  ```
- [ ] Step 2：`cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test trust`，增加正确签名但过期/撤回/回放、未知 key、未经旧 root 授权换根、时钟回退与离线旧安装重启的独立红例。
- [ ] Step 3：依 G2 实现逐字节签名验证和持久化 trust 更新；只在整体验证成功后推进接受序号。坏元数据不能污染 trust store，下载端不能选择公钥。`bootstrap` 仅接收宿主内置/编译时固定的根锚，首次网络返回的 root envelope 不能自签成为新锚；后续换根必须经已有根授权。测试 fixture 可用测试根调用，正式构建检查禁止嵌入测试根。
- [ ] Step 4：验证所有负例通过；修改一个签名检查条件，确认 tamper 负例红，再恢复。输出错误不得包含密钥或原始认证信息。
- [ ] Step 5：按 C 提交 `feat: verify official plugin release trust`。

### Task T08：实现有界安全解包与不可变安装目录

**前置：** T07。

**Files:** Create `yi-agent-plugins/src/bundle.rs`、`src/limits.rs`、`tests/bundle.rs`。Modify lib 导出。

**Interfaces:** 实现 `ExtractLimits::default`、`install_verified`、`InstalledRelease`；输入 archive 的摘要、大小和内容均须匹配 `VerifiedRelease`。

- [ ] Step 1：创建 traversal、绝对路径、软/硬链接、设备文件、重复名、大小写碰撞、超大解压量夹具，确认它们不在 bundle 测试里被当作普通文件接受。
- [ ] Step 2：增加失败测试的核心断言：
  ```rust
  assert!(install_verified(&verified, &hostile_archive, &destination,
      &ExtractLimits::default()).is_err());
  assert!(!outside.join("escaped").exists());
  assert!(!destination.exists());
  ```
  `verified`、`hostile_archive`、`destination`、`outside` 均在该测试中通过 fixture 签名器/TempDir 创建；不能复用正常包签名来让所有攻击仅因摘要不符失败。
- [ ] Step 3：实现 tar.gz 流式限量读取、逐 entry 校验、相对入口校验、独立临时目录、同文件系统原子落盘；失败清理临时产物。不得先 `extract` 全包再检查。归档中的 uid/gid、setuid/setgid 和原始权限不直接采纳；仅对清单声明的原生入口由宿主设置受控执行位，其他静态文件只读。
- [ ] Step 4：`cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test bundle`；验证磁盘写失败、取消、超限压缩流和安装后文件被替换；每个攻击都使用有效测试签名，确保测到解包边界。
- [ ] Step 5：按 C 提交 `feat: install verified plugin bundles safely`。

### Task T09：实现实例注册表、安装幂等与跨进程互斥

**前置：** T08。

**Files:** Create `yi-agent-plugins/src/registry.rs`、`src/locks.rs`、`tests/registry.rs`。Modify lib 导出。

**Interfaces:** 实现 `Registry::open/record_install`；增补 `Registry::list() -> Result<Vec<PluginRecord>, PluginError>`，`PluginRecord` 包含已安装版本、选中/激活状态和项目授权摘要。实现生命周期独占锁与短期 registry 锁，不持短锁做网络/等待。

- [ ] Step 1：写同摘要重复安装幂等、同版本不同摘要冲突、坏 registry 不覆盖、两个进程并发安装的失败测试。
- [ ] Step 2：`cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test registry`，确认读者看不到半个 JSON 或未完成版本目录。
- [ ] Step 3：实现带 schemaVersion 的原子记录、唯一临时文件、必要 fsync/目录 fsync、进程锁与坏数据显式错误；首次安装只设选中版本，不创建 grant 或启动记录。
- [ ] Step 4：用 subprocess 真正争抢同一 registry，并验证不同 `instance_home` 不串；不能只用同进程 Mutex 测跨进程安全。
- [ ] Step 5：按 C 提交 `feat: track managed plugin installations per instance`。

### Task T10：实现项目授权、双代次与能力分类

**前置：** T09。

**Files:** Create `yi-agent-plugins/src/grants.rs`、`src/project.rs`、`tests/grants.rs`。Modify registry/types。

**Interfaces:** 实现 `grant/check`、规范项目 ID/句柄、独立项目授权代次。业务 method/CLI effect 由签名清单解析，不接受客户端把 write 标成 read。

- [ ] Step 1：加入跨项目、跨宿主、已停用旧 binding、同版本重启旧 binding、仓库伪造 supervisor/flag 的失败测试。
- [ ] Step 2：核心断言：
  ```rust
  assert!(registry.check(&old_project_a, Effect::Write).is_err());
  assert!(registry.check(&unchanged_project_b, Effect::Write).is_ok());
  assert!(registry.check(&read_only_binding, Effect::Launch).is_err());
  ```
  测试在两个不同临时项目创建授权，只撤销 A；fixture 不允许用相同 ProjectId 代替两项目。
- [ ] Step 3：实现能力差异比较与重新确认状态；把配额修改、自动任务、常驻分别授权。用户未批的新权限不得因升级获得；正常旧权限不无故重置。
- [ ] Step 4：`cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test grants`；覆盖真实 symlink 规范化、未登记 worktree、同名目录及授权持久化后重开。
- [ ] Step 5：按 C 提交 `feat: authorize plugin projects with scoped bindings`。

### Task T11：实现活动准入、排空屏障与幂等操作记录

**前置：** T10。

**Files:** Create `yi-agent-plugins/src/admission.rs`、`src/operations.rs`、`tests/admission.rs`、`tests/operations.rs`。

**Interfaces:** 实现 §3 Admission API。新增 `OperationLedger::result(request_id: &str, binding: &Binding) -> Result<OperationOutcome, PluginError>`；`OperationOutcome` 与 TS result 联合一一对应。读操作不留下永久幂等日志。

- [ ] Step 1：写排空期间新 Launch/外部 Write 拒绝、旧 turn 的状态写与锁释放允许、turn 结束后旧凭据不能开下一轮的失败测试。
- [ ] Step 2：`cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test admission && cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test operations`；先验证保护缺失时确实红。
- [ ] Step 3：实现准入与屏障的原子检查、TurnPermit/OperationPermit 存活锁、规范 request ID 与参数摘要；重复同 ID 同参数不重复提交，重复 ID 改参数返回 Conflict。已送出但缺结果的写标 Unknown，不在重连/重启时重放。`parent_turn` 字符串不是收尾授权：须核对宿主登记的会话/调用者绑定和仍存活的屏障前 permit，伪填他人 turn ID 的外部写必须拒绝。结果查询与新业务准入分离：认证管理调用可在原绑定作用域查询已提交结果，旧代次不能据此再提交操作，切换项目不能取另一个项目结果。
- [ ] Step 4：subprocess 测进程崩溃释放、暂停进程不能按 TTL 回收、活跃审批仍阻塞 drain、单 turn 多次必要收尾以及并发建立屏障。取消 drain 恢复旧准入，不能关闭整个插件。
- [ ] Step 5：按 C 提交 `feat: gate plugin operations and drain active turns safely`。

## Phase 3：生命周期与宿主运行接入

### Task T12：实现候选激活、事务日志与失败恢复

**前置：** T11。

**Files:** Create `yi-agent-plugins/src/lifecycle.rs`、`src/journal.rs`、`src/migration.rs`、`tests/lifecycle.rs`、`tests/recovery.rs`。

**Interfaces:** 实现 §3 Lifecycle API、G2 phase 表、RuntimeControl trait。测试提供 `RecordingRuntime`，其实现逐项记录 maintenance/start/stop/health 动作并按指定阶段返回错误，不空实现成总成功。

- [ ] Step 1：状态机测试先断言候选不可被普通 binding 使用、一个项目迁移失败时全部项目不恢复新准入、cancel 只在可安全退回阶段恢复。
- [ ] Step 2：恢复测试使用表驱动循环覆盖 G2 每个持久化 phase：注入崩溃 → 重开 registry/lifecycle → recover → 断言唯一下一步及准入状态。
- [ ] Step 3：实现备份清单和完整性、幂等迁移/从备份重做、候选维护凭据、全部健康后原子提交、修复投影后开放准入；任何不可恢复状态保持 RecoveryRequired。
- [ ] Step 4：`cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test lifecycle && cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test recovery`。断言恢复前没有 start/launch 发生，备份不包含 socket/PID/活锁，恢复不覆盖别人改过的共享配置修订。
- [ ] Step 5：按 C 提交 `feat: activate plugin versions with recoverable transactions`。

### Task T13：实现整机配额域适配与项目维护所有权

**前置：** T12、G3。

**Files:** Create `yi-agent-plugins/src/quota.rs`、`src/ownership.rs`、`tests/quota.rs`、`tests/ownership.rs`。Modify `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lease.rs`、`service.rs`、`dispatch.rs`、`resident.rs`。

**Interfaces:** `QuotaContext { config_path, leases_path, revision }` 和 `ProjectOwnership` 由宿主注入，独立于 instance_home。配额共享格式遵循 G3，不能让一个新实例创建自己的完整预算。跨进程活动观测带 owner 存活锁；只在死亡确认后回收，不按过期时间猜。

- [ ] Step 1：两实例/两项目测试同一峰值，不允许各自再得到全额；修改共享配置产生修订冲突时更新/回退失败，不覆盖新值。
- [ ] Step 2：增加旧格式共存与不兼容迁移拒绝、无法定位共享域拒绝新自动任务、同项目第二 owner 拒绝、维护期间后端重启 owner 不丢失的红例。
- [ ] Step 3：实现显式上下文和跨实例兼容检查；运行统计不能把“宿主本进程的 sessions.running”直接当整机值。聚合已登记活 owner 的统计且不双扣看板租约；quota 中审批等待的既有口径与 drain 中未结束 turn 的口径分别保留。
- [ ] Step 4：`cd yi-agent-rs && cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test quota && cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test ownership`；再 `cd ../plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`。覆盖普通非看板回合消耗预算，实例关闭不留下虚假活动。
- [ ] Step 5：按 C 提交 `feat: preserve shared plugin quotas across managed instances`。

### Task T14：接入 supervisor、watchman 与受管 CLI

**前置：** T13。

**Files:** Create `yi-agent-rs/crates/yi-agent-supervisors/src/managed.rs`、`yi-agent-rs/crates/yi-agent/src/plugins_cmd.rs`、`yi-agent-rs/crates/yi-agent/tests/plugins_exec.rs`、`yi-agent-rs/crates/yi-agent-store/src/ipc/managed.rs` 及相应 IPC 集成测试。Modify supervisors 的 `lib.rs`、`supervisor.rs`、`refresh.rs`、`watch.rs`、`watchman.rs`、`daemon.rs`，CLI `src/config.rs`（Cli/Command 和 PluginsAction）、`main.rs`、`control_commands.rs`，store `src/ipc/protocol.rs`、`daemon.rs`、`mod.rs` 及相应 Cargo manifests。store adapter 可依赖无 store 依赖的核心 crate，不能反向引入 app-server。

**Interfaces:** 实现 G1 受管 CLI；guardian 在 backend 重启期间持有 ProjectOwnership 并尊重 maintenance。所有受管启动先检查 registry、binding、文件完整性及许可，legacy supervisor 的一般逻辑不全局重写。现有 daemon 是独立进程：G2 必须定义维护请求/确认、事务 ID 与代次关联及 IPC 兼容策略，T14 将其接入 store 的实际 dispatcher。daemon 在启动、重连和每次重启决策前读取持久化屏障/恢复状态；RPC 通知丢失不能成为放行理由。旧 daemon 不支持维护协议时阻止激活并提示显式升级/停止，不能把解析错误视为已暂停。

- [ ] Step 1：CLI subprocess 测试删除 PATH 中插件/yi-agent 后，以 `CARGO_BIN_EXE_yi-agent` 绝对路径仍能运行 counter；参数含空格/中文/分号只作为 argv，不被 shell 执行。
- [ ] Step 2：supervisor 测“暂停维护时不重启”“旧 manifest 自述不得覆盖受管 launcher”“无授权不执行 command”“两个实例不能同时守护同项目”，先红后绿；启动真实 detached daemon，注入维护 IPC 丢包、旧协议拒绝和管理端崩溃，证明不抢跑。
- [ ] Step 3：接入 launcher 和 guardian，保留宿主控制的 `YI_AGENT_HOME`、binding/代次/配额路径；插件 env 不得覆盖保留变量。健康检查只给候选维护凭据，不开放公共 CLI；CLI enum、dispatch、daemon dispatcher、协议握手及 `--help` 测试一起接线。按 G2 固定的失败窗口、重启次数和退避上限实现熔断，达到阈值关闭新自动任务并显示后端故障；用注入时钟测崩溃循环与手动恢复，不能无限重启或把后端失败显示为空数据。
- [ ] Step 4：`cd yi-agent-rs && cargo test -p yi-agent --test plugins_exec && cargo test -p yi-agent-supervisors`；额外用真实子进程模拟 launch 后管理进程中断，确认不存在一边 rollback 一边继续启动新服务。
- [ ] Step 5：按 C 提交 `feat: launch managed plugins through guarded host entrypoints`。

### Task T15：接入会话、driver、skill 与恢复路径

**前置：** T14。

**Files:** Create `yi-agent-rs/crates/yi-agent-app-server/src/plugin_manager/mod.rs`、`sessions.rs`、`skills.rs`、`server/plugin_binding_tests.rs`。Modify `src/lib.rs`、`server.rs`、`rpc/thread.rs`、`rpc/turn.rs`、`session.rs`、`thread_store.rs`、`session_driver.rs`、`skill_rpc.rs`；`yi-agent-skills/src/service.rs`、`discovery.rs`；CLI `src/resume.rs` 和 `main.rs` 的会话装配。

**Interfaces:** ThreadMeta 保存 `Vec<Binding>`（兼容旧缺省空），活跃 turn 持有 TurnPermit；会话环境携带绝对 launcher 与受管绑定路径，保留普通 shell 权限边界。skill registry 接收显式托管来源，不伪造 User/System scope。

- [ ] Step 1：mock driver 试图在没有 Launch 权限、drain 屏障后、错误项目 workdir 发起任务，都应被拒绝；现有 tools/合并审批不得跳过。
- [ ] Step 2：测试新会话获取正确 skill、同名副本阻止自动任务、旧会话正常聊天但调用插件返回 StaleBinding、resume 不丢版本绑定。CLI 和桌面线程建模两条路径均覆盖，不只改 app-server。
- [ ] Step 3：接入 begin_turn/finish 与命令许可的实际生命周期，包括 AwaitingApproval、cancel、模型失败、会话重建、后台 driver 和普通用户追加回合；所有路径释放正确 permit，不在 Agent::run 重置 token 前捕获错误 cancel token。
- [ ] Step 4：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib plugin_binding && cargo test -p yi-agent-skills && cargo test -p yi-agent --test plugins_exec`。检查筛选结果非零；回归现有 session_driver、resume、thread 元数据兼容测试。
- [ ] Step 5：按 C 提交 `feat: bind plugin skills and sessions to managed releases`。

## Phase 4：管理界面、隔离桥与远程资源

### Task T16：实现通用插件管理 RPC 和下载协调

**前置：** T15。

**Files:** Create `yi-agent-app-server/src/plugin_manager/catalog.rs`、`downloads.rs`、`rpc.rs`、`runtime.rs`、`server/plugin_management_tests.rs`。Modify `src/rpc/mod.rs`、`rpc/plugin.rs`、`server.rs`、`protocol.rs`、CLI `yi-agent-rs/crates/yi-agent/src/config.rs`、`plugins_cmd.rs`、`tests/plugins_exec.rs`、`desktop/src/lib/routes.ts`、`routes.test.ts`。CLI 复用该 crate 对外暴露的管理应用服务，不复制 HTTP/验证/事务实现。

**Interfaces:** 实现 §3 的完整 `plugins/v1/*` 管理 API；app-server 的 RuntimeControl 适配 guardian/driver，不复制生命周期状态机。下载器输入受信发布标识，不能接受 iframe 传入任意 URL 或本地路径。

- [ ] Step 1：测试 install 不调用 enable/daemon/launchd/manifest，enable 失败只回滚本次授权，权限增加等待确认，legacy plugin/install 不能绕过新管理流程。
- [ ] Step 2：测试同一事务在两个客户端看到相同进度，CLI 与 RPC 争用统一锁；取消下载/磁盘不足/HTTP 重定向离开允许源明确失败，旧版本不受影响。本地包导入缺元数据、过期、错签拒绝；有效官方签名包导入仍不启用项目。headless subprocess 完成安装、启用、激活、查询与卸载，不能只测 exec。
- [ ] Step 3：实现认证目录与流式下载，限制 HTTPS 官方来源及受认证宿主资源传输；超时分为无进展超时与总超时，使用注入时钟/fetcher 测试，不用长 sleep。管理进度/错误脱敏。自动检查只刷新受信元数据和可用更新提示，退避/离线不影响已有版本，绝不自动 activate；本地导入仍验证同一信任根、序号、大小与摘要。
- [ ] Step 4：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib plugin_management`；运行 bindings-check 及 routes 测试；验证后台常驻只有显式授权时调用现有 resident adapter。
- [ ] Step 5：按 C 提交 `feat: expose versioned plugin management operations`。

### Task T17：实现生产资源加载与原生恢复容器

**前置：** T16、G0。

**Files:** Create `desktop/src-tauri/src/plugin_host/assets.rs`、`cache.rs`、`container.rs`、`recovery.rs`、`tests.rs`。Modify `plugin_host/mod.rs`、`src/lib.rs`、`src/bridge.rs`、`tauri.conf.json`、`capabilities/default.json`、桌面 Rust manifest。

**Interfaces:** 消费 `VerifiedRelease/InstalledRelease`；桌面可复用无 sqlite 的核心 crate 做验证。资源 API 只接宿主内部视图句柄，不能让 iframe 调任意 `read_file(path)`。产出 view instance ID、受控静态入口和原生关闭/恢复句柄。

- [ ] Step 1：Rust 测试缓存损坏、包外路径、跨包/版本 token、已撤销代次、symlink 文件替换拒绝；正常 Unicode 安装根可用。
- [ ] Step 2：重跑 T01 probe，将其改为调用生产 container/assets 模块，检查 containerConfigSha256 与发布配置一致。
- [ ] Step 3：实现包独立验证与只读缓存、代次绑定的资源服务和原生恢复；CSP 不再 `null`，对主窗口已有合法资源需求做回归，不用全开 `*` 修报错。
- [ ] Step 4：`cargo test --manifest-path desktop/src-tauri/Cargo.toml plugin_host`，以及正式 probe isolation suite；不能只跑 Rust MockRuntime 就声称原生 invoke 被拦截。
- [ ] Step 5：按 C 提交 `feat: load verified plugin views in isolated containers`。

### Task T18：实现宿主桥与独立 SDK

**前置：** T17。

**Files:** Create `desktop/src/lib/pluginHost/types.ts`、`bridge.ts`、`bridge.test.ts`、`events.ts`、`events.test.ts`；`plugins/sdk/package.json`、`package-lock.json`、`tsconfig.json`、`vitest.config.ts`、`src/index.ts`、`src/index.test.ts`。Modify contract checker 的 TS golden 对照。SDK package 名为 `@yi-agent/plugin-sdk`，固定初始 API 版本，`exports` 和 `types` 指向 build 生成的 dist，scripts 明确提供 test/build；不让后续 UI 临时从 SDK 的 TS 私有路径导入。

**Interfaces:** 实现 §3 `PluginHost`。SDK 只解析桥上下文/发送消息，不带宿主 RPC client 或 Tauri import。宿主绑定 frame/MessagePort、宿主身份、双代次、项目句柄；消息无法选择另一插件。

- [ ] Step 1：桥测试：
  ```ts
  it('does not trust plugin-supplied identity', async () => {
    const reply = await bridge.receive(frameB, messageForFrameA);
    expect(reply.error.kind).toBe('PermissionDenied');
    expect(forwardToBackend).not.toHaveBeenCalled();
  });
  ```
  测试内使用两个实际不同的 fake frame/source 与同一协议黄金消息，不能把身份校验 mock 掉。
- [ ] Step 2：增加重放初始化、旧项目响应、重载后写不重放、重复 request ID 改参数、超限/未知方法、事件缺修订先取快照的失败测试。
- [ ] Step 3：实现受控 MessageChannel 初始化、限量请求表、取消等待语义、operationResult、主题/项目变更、订阅清理、频率限制与 openThread 范围检查；SDK 构建与宿主源码隔离。
- [ ] Step 4：`(cd desktop && npx vitest run src/lib/pluginHost && npm run build) && npm ci --prefix plugins/sdk && npm test --prefix plugins/sdk && npm run build --prefix plugins/sdk`；contract golden 双端一致。
- [ ] Step 5：按 C 提交 `feat: add scoped plugin bridge and standalone sdk`。

### Task T19：用第二插件验证发现、导航、设置与授权 UX

**前置：** T18。

**Files:** Create `plugins/fixtures/example-counter/plugin.json`、`ui/index.html`、`ui/main.js`、`skills/example-counter/SKILL.md`、独立 workspace `backend/Cargo.toml`、`backend/Cargo.lock`、`backend/src/main.rs`、`backend/tests/protocol.rs`；`desktop/src/components/PluginView.tsx`、`PluginView.test.tsx`、`PluginCatalog.tsx`、`PluginCatalog.test.tsx`、`desktop/src/hooks/usePlugins.ts`、`usePlugins.test.ts`。Modify `App.tsx`、`SettingsPluginsTab.tsx`、`SettingsDialog.tsx`。

**Interfaces:** 清单贡献生成主视图与设置入口；PluginCatalog 只调用管理 API，PluginView 只使用受管 view handle。example-counter 的 read/write method 使用不同能力，验证宿主无需知道看板。counter 原生 fixture 按 G1 实现维护健康检查、无副作用的运行描述与 read/increment，并通过受管数据路径存储计数；其独立 Rust workspace 用于开发/验收编译，不要求用户安装 Rust。`cargo test --manifest-path plugins/fixtures/example-counter/backend/Cargo.toml` 必须先证明协议和幂等，正式 WebView 不能请求不存在的 counter 后端。

- [ ] Step 1：写 UI 失败测试：未安装/已安装未启用/只读启用/运行故障/更新等待/待清理逐个有独立状态，不用一个 enabled 布尔；安装按钮不能隐式开常驻。
- [ ] Step 2：counter 清单增加第二设置入口，宿主不改注册代码即显示；安装前只渲染静态介绍，业务 iframe 不创建。
- [ ] Step 3：实现目标宿主提示、原生信任提示、基本数据/自动任务/配额/常驻分项授权、依赖冲突诊断、错误及事务进度。停用后有宿主管理入口但不能继续调用业务后端。
- [ ] Step 4：`cd desktop && npx vitest run src/components/PluginView.test.tsx src/components/PluginCatalog.test.tsx src/hooks/usePlugins.test.ts src/components/SettingsPluginsTab.test.tsx && npm run build`；正式 WebView 加载 counter 主视图/设置页。
- [ ] Step 5：按 C 提交 `feat: manage official plugins through generic desktop views`。

### Task T20：接入远程精确版本资源与多客户端撤销

**前置：** T19。

**Files:** Create `desktop/src-tauri/src/plugin_host/remote_assets.rs`、`desktop/src/lib/pluginHost/remote.test.ts`、`yi-agent-app-server/src/server/plugin_remote_tests.rs`。Modify `desktop/src-tauri/src/bridge.rs`、`remote/mod.rs`、`plugin_host/cache.rs`；`plugin_manager/rpc.rs`、`runtime.rs`。

**Interfaces:** 远端 `assetRelease` 返回已认证的发布身份和受控包传输句柄，不返回供页面直接 fetch 的宿主凭据。缓存字节可去重；运行绑定始终包含远端宿主/代次。无桌面的安装与更新可用。

- [ ] Step 1：两宿主同插件不同版本测试：切换到 B 不能展示 A 的 UI，错误摘要/撤回/过期新发布拒绝；远端平台二进制绝不在桌面 spawn。
- [ ] Step 2：测试旧客户端不兼容只禁该视图、headless 更新可用、离线有匹配已验证缓存可恢复原授权视图、无缓存明确不可加载。
- [ ] Step 3：实现独立签名/包校验、完整平台包静态资源提取、重连握手、旧通道撤销、连接切换清理和有界客户端关闭确认；断线写查结果，不重新提交。
- [ ] Step 4：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib plugin_remote`；`cd ../desktop && npx vitest run src/lib/pluginHost/remote.test.ts`；用两个真实隔离 app-server 连接完成远程验收，不能仅把两个对象命名成两宿主。
- [ ] Step 5：按 C 提交 `feat: bind plugin views to remote host release identities`。

## Phase 5：迁移完整看板插件

### Task T21：迁移看板主视图与设置到独立 UI 工程

**前置：** T20。

**Files:** Create `plugins/superpowers-kanban/ui/package.json`、`package-lock.json`、`tsconfig.json`、`vite.config.ts`、`vitest.config.ts`、`src/test/setup.ts`、`index.html`、`src/main.tsx`、`src/host.ts`、`src/styles.css`，Create 看板根 `plugin.json`。Copy 后于 T25 删除 desktop 下 `SuperpowersKanbanBoard/Card/Column/Enqueue/PluginSettings/Settings/View.tsx` 及其对应现有测试到 `ui/src/components/`；`useBoardState.ts` 到 `ui/src/hooks/`，`boardIndex.ts`、`boardRefresh.ts`、`superpowersKanbanBoard/Boards/State/Switch.ts` 及对应现有测试到 `ui/src/lib/`。拆分 `desktop/src/lib/pluginSettings.ts` 及其测试：业务数据转换归插件、通用管理归宿主；`errorMessage.ts`、`connectionError.ts` 中所需语义在插件独立实现并测错误映射；`desktop/src/ui/Popover.tsx` 的所需交互在插件自有组件实现，带键盘/焦点测试，不依赖宿主 CSS。Modify desktop App/Settings 通用入口接线。

**Interfaces:** UI 中 `host.ts` 从 SDK 获得 PluginHost，不再 import `desktop/src/lib/rpc`；业务 backend adapter 只调用声明的看板 method，打开会话和主题用宿主能力。UI 更新无需重编主程序。SDK 用固定版本的本地 `file:../../sdk` 开发依赖和 lock，SDK 先构建出声明/JS；发布 UI 是自包含静态产物，不要求用户机器存在 SDK 目录。原 RpcCall/routes/RpcClient 依赖改为插件自己的 adapter 类型和声明 method 常量，不复制整个宿主网络客户端。

- [ ] Step 1：先复制而不立即删除宿主组件，在独立工程跑原有卡片/设置/刷新测试，记录它们因私有 import 失败的清单。
- [ ] Step 2：加入结构测试禁止 `desktop/src`、`@tauri-apps`、宿主私有 CSS/store/RPC import；依赖的 React/markdown/style 包显式记入新 lock，使用主题令牌补齐样式，不通过复制整个 App store 解决。
- [ ] Step 3：改造主视图、设置、入队、看板项目选择与事件刷新；写操作带 requestId 并处理 Unknown，断线不画空看板。业务设置中的常驻和共享配额请求转为受控宿主能力，不由插件改 registry 文件。
- [ ] Step 4：`npm ci --prefix plugins/sdk && npm run build --prefix plugins/sdk && npm ci --prefix plugins/superpowers-kanban/ui && npm test --prefix plugins/superpowers-kanban/ui && npm run build --prefix plugins/superpowers-kanban/ui`；在同一宿主安装新 UI 包验证卡片、时间窗口设置、入队和会话跳转。Vitest 用 jsdom + React setup，测试计数不低于迁移前保留的业务用例；缺 import/样式依赖逐项闭合。此阶段允许源码短暂双份，但不作为最终发布物。
- [ ] Step 5：按 C 提交 `feat: build kanban views as a standalone plugin ui`。

### Task T22：接入看板受管后端、命令、幂等与工作流依赖

**前置：** T21、T15。

**Files:** Create `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/managed.rs`、`operations.rs`。Modify `main.rs`、`service.rs`、`dispatch.rs`、`tick.rs`、`resident.rs`、`lease.rs`、`skills/superpowers-kanban/SKILL.md`、`supervisors/superpowers-kanban.json`、`contract/managed-v1.json`、两份现有 driver/socket contract。

**Interfaces:** 使用 G3 的 effect/上下文与受管 launcher；插件对宿主 Rust crate 仍零链接，通过公开 JSON/CLI 契约通信。`state --self` 的卡号保持插件字段，项目归属取宿主 binding，不取 worktree cwd。写 requestId 传递到插件操作日志。

- [ ] Step 1：增加回归：上游依赖缺失不 launch；旧 turn 排空时 state/merge-finish 可用；环境保留键不能由插件覆盖；管理常驻不直接改宿主文件。
- [ ] Step 2：幂等测试同 requestId 不能重复创建卡片/重复授权合并；故障注入在写入后回包前中断，重复查询返回 committed/unknown 而不是再投递；不承诺任意外部 git 操作 exactly-once。
- [ ] Step 3：更新 skill 全命令表为宿主受管入口约定，保留状态机和用户合并确认；同步现有 `the_skill_*` 测试，不删掉命令覆盖。旧独立 CLI 可保留显式 standalone 模式，但不把它当受管权限证明，也不让其并跑已托管项目。
- [ ] Step 4：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core && cargo test -p superpowers-kanban-ipc && cargo test -p superpowers-kanban-runner`；再验证新老 host contract 正反例与 mock 会话完成、合并及配额流程。
- [ ] Step 5：按 C 提交 `feat: run kanban workflows through managed plugin contracts`。

### Task T23：实现旧安装接管和数据迁移

**前置：** T22。

**Files:** Create `yi-agent-plugins/src/legacy.rs`、`tests/legacy.rs`、`plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/migrate.rs`、`tests/managed_migration.rs`。Modify 看板 `plugin.json`、`plugin_manager/rpc.rs`、`PluginCatalog.tsx`。

**Interfaces:** 通用 legacy 规则来自签名清单，仅在用户选择/已登记项目及已知 skill 根探测；显式迁移入口来自已验证包。旧可执行文件不能被运行来“认证自己”。

- [ ] Step 1：为旧 supervisor+PATH+skill+卡片数据+常驻引用构造完整临时安装；接管前拒绝自动运行新程序，未知数据格式不原地猜。
- [ ] Step 2：测试 ID、状态、队列顺序、会话关联和配置保持；用户自定义 skill/旧二进制不删除；活锁不复制；旧开关不等于已获新授权。
- [ ] Step 3：实现备份、排空旧操作、停止旧服务、用户重批权限、原子接管与恢复。旧 guardian 与新 owner 冲突时报告阻塞，不 pkill 名称匹配的其他实例。
- [ ] Step 4：`cd yi-agent-rs && cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test legacy`；`cd ../plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --test managed_migration`；逐阶段中断验证无新旧并跑。
- [ ] Step 5：按 C 提交 `feat: adopt legacy kanban installs without losing project data`。

### Task T24：完成停用、卸载、清理与保留数据策略

**前置：** T23。

**Files:** Create `yi-agent-plugins/src/uninstall.rs`、`src/gc.rs`、`tests/uninstall.rs`、`tests/gc.rs`。Modify lifecycle、管理 RPC、PluginCatalog 与相关测试。

**Interfaces:** 停用/卸载复用 Admission 和 Lifecycle，不另造一套杀进程逻辑。GC 输入受管引用清单，保护旧会话可读 skill、事务、备份、活进程与其他实例共享资源。

- [ ] Step 1：活动回合卸载必须等待/允许用户显式取消，用户拒绝时恢复旧状态；停止写准入后新任务不能穿过。
- [ ] Step 2：测试默认保留数据、显式 clear 列明项目范围、项目不可达留下 pending cleanup、其他插件常驻引用保留、共享配额不删除、卸载后旧 supervisor 不启动。
- [ ] Step 3：实现按引用清理、可见空间占用与手工清理入口；保留数据清单不是运行授权。重装只在明确选择项目、校验格式后重新接管，不自动恢复过去授权。
- [ ] Step 4：`cd yi-agent-rs && cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test uninstall && cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test gc`；UI 回归停用/待清理与错误提示。
- [ ] Step 5：按 C 提交 `feat: uninstall managed plugins without deleting user data`。

### Task T25：移除宿主看板专属路径并验收独立发布

**前置：** T24。

**Files:** Delete 已迁移的 `desktop/src/components/SuperpowersKanban*.tsx` 及测试、`desktop/src/hooks/useBoardState.ts`、迁移后的看板 lib/测试；`yi-agent-rs/crates/yi-agent/src/tui/superpowers_kanban.rs`。Modify `desktop/src/App.tsx`、`App.test.tsx`、`components/SettingsPluginsTab.tsx` 及测试、`SettingsDialog.tsx`、`ThreadSidebar.tsx`、`hooks/useHandshakeRecovery.ts`、`lib/pluginSettings.ts` 及测试；TUI `plugins.rs`、`slash.rs`、`mod.rs`、CLI `main.rs`、`yi-agent-flags/src/registry.rs` 和对应测试、系统 skill/help 中实际搜索命中的注册文件，以及 app-server 测试对插件目录 contract 的引用。Create `scripts/plugins/check-host-boundary.py`、`test_check_host_boundary.py`。执行前用 `git grep -n -E 'superpowers.kanban|SuperpowersKanban|useBoardState|KANBAN_PLUGIN' -- desktop/src yi-agent-rs/crates` 记录逐命中处置表；新增命中也必须归类，不能只删本表文件。

**Interfaces:** 主程序只保留通用插件贡献/命令。看板卡片语义由插件持有；宿主驱动会话分组保留通用 driver 字段，不能因删除看板 UI 丢失其他驱动会话。

- [ ] Step 1：边界扫描测试对带有旧 import、硬编码注册、看板私有 flag 的人工宿主 fixture 必须失败；对 ID 仅作为测试数据/迁移声明或插件目录内容不误报。新增第二插件无需改扫描规则。
- [ ] Step 2：删除宿主看板组件和注册，切换 App 到通用视图；TUI 用通用插件 CLI 帮助，不在核心保留 `/kanban` 私有业务 switch。插件内业务测试迁移保留，不直接删测试数量来过关。
- [ ] Step 3：`python3 -m unittest discover -s scripts/plugins -p 'test_check_host_boundary.py' && python3 scripts/plugins/check-host-boundary.py`；build 主程序后检查发布物不含业务资源、程序、skill。宿主测试 contract 改用独立公共 fixture，避免编译主程序必须带插件源码：实际命中包括 app-server `src/session_driver.rs`、store `src/ipc/protocol.rs` 和 supervisors `src/manifest.rs`。主程序不复制插件业务 schema；插件 workspace 反向验证公共 driver/socket 契约兼容。
- [ ] Step 4：构建看板 1.0/1.1 fixture 包（版本号为测试版本，不修改正式发布版本），只改插件 UI 标记并更新插件包版本；在同一 `.app` 上安装/更新，断言 UI 标记变化而 appSha256 不变。
- [ ] Step 5：按 C 提交 `refactor: remove kanban business paths from the host`；同步 desktop、app-server、TUI、skills/flags 和看板模块的实际完成条目。

## Phase 6：完整发布与证据门禁

### Task T26：打包、独立 CI 与安全发布流水线

**前置：** T25。

**Files:** Create `yi-agent-plugins/src/bin/plugin-pack.rs`、`tests/pack.rs`、`scripts/plugins/build-release.sh`、`verify-release.sh`、`test_release_scripts.py`、`.github/workflows/plugins.yml`、`plugin-release.yml`、新插件管理用户文档 `docs/plugins.md`。Modify 新 crate manifest、`.github/workflows/ci.yml`、`.gitlab-ci.yml`、`yi-agent-rs/justfile`、看板 README/INSTALL。

**Interfaces:** `plugin-pack` 使用同一 manifest/trust/bundle 契约，确定性归档后签名；私钥从受保护文件读取，不进 argv、日志、仓库或前端。目录发布在所有包上传/校验成功之后，禁止先发布目录再上传包。

- [ ] Step 1：pack round-trip 红例：有效包经生产 verifier/extractor 安装；单字节篡改失败；相同输入两次包内容一致。跨平台 UI/skill 摘要相同，原生 executable 分别检查。
- [ ] Step 2：release script 单测确保插件 tag 不触发主程序版本流水线、检查 tag 在 main 且匹配插件 workspace/UI 版本、缺平台产物不更新目录、主程序版本未变化。插件 tag 采用 `plugin-superpowers-kanban-v*.*.*`，不冒用主程序 `v*.*.*`。
- [ ] Step 3：新增独立插件 Rust、UI、SDK、contracts 与脚本检查 recipe/jobs；Node 检查放独立 job，不把 Node 强塞进无 Node 的主 Rust `just ci`。云端不可信 PR 不能接触 self-hosted runner 或签名密钥；签名发布仅受保护 main/tag 环境审批。
- [ ] Step 4：依目标平台签名/公证程序后再归档/签插件元数据，验证真实下载后的 OS 执行策略。构建正式主程序时默认不启用 probe、不携带 fixture/测试 root；正式插件包不含测试密钥/攻击页/开发端口。不向用户建议关闭 Gatekeeper。
- [ ] Step 5：按 C 提交 `ci: build and verify independently versioned plugin releases`。本任务只实现发布机制；真正上传、签名生产包和发布目录必须再次获发布授权。

### Task T27：完成 A01–A20 正式验收与交付

**前置：** T26，所有 G 门槛已确认，B0 解除。

**Files:** Create `scripts/plugins/e2e.mjs`、`e2e-report.test.mjs`、`acceptance-cases.json`、`docs/superpowers/reports/2026-11-02-kanban-independent-release-acceptance.md`。Modify probe runner 复用模块、CI 插件验收 job 和项目进度文档。

**Interfaces:** `e2e.mjs --suite all --output .artifacts/plugins/acceptance.json` 构建/启动隔离实例，使用真实 Tauri WebView、本机两个独立 app-server 和本机认证网络远端连接完成 A16/A17，不触碰真实账号/LLM。平台差异需要对应平台报告，不把交叉编译代替运行。证据分为 probe 构建的攻击注入报告与不启用 probe 的正式发布物验收：后者由宿主外部 OS UI 自动化/人工可复核操作及管理 CLI 采集，不给正式包新增测试 invoke、报告后门或测试信任根。正式平台/官方签名包验收需要另行授权的签名 release candidate；未授权只运行测试根构建并记“尚无正式包证据”，不能据此把 A20/整体 feature 标完成。

- [ ] Step 1：报告校验测试确保 A01–A20 各有且仅有一个最终结果，任何缺项、skipped、unsupported、timeout、失败退出均非通过；mutation 删除 A07 或令 A10 缺 evidence，checker 必须失败。
- [ ] Step 2：按矩阵执行安全/故障注入，包括断线提交、旧 frame 重放、两实例预算、活跃审批、更新收尾、候选不可见、共享配置修订冲突、旧安装接管和 pending uninstall；所有数据为 TempDir/专属 `YI_AGENT_HOME`。
- [ ] Step 3：至少做一次实际可访问的远程执行宿主连接验证（认证、目标 OS/架构与下载执行），记录两端身份和版本；不能只凭 loopback 证明跨机器行为。无真实远端证据时 A16/A17 保持未完成，阻止发布但可保留已通过的本机报告。
- [ ] Step 4：在相同主程序包上执行插件 UI v1→v2 更新；比对主程序整个发布物摘要、插件摘要、活跃版本/代次和 skill 来源；卸载后验证核心聊天和默认数据保留。保存脱敏报告、截图、平台版本及每项对应测试命令。
- [ ] Step 5：运行 C、所有独立 workspace/前端/脚本/正式平台门禁，审阅证据后提交 `test: verify independent kanban plugin delivery end to end`。仅此时将完整独立发布 feature 标 `[x]`；执行合并后再次验证，再按仓库规则回收不再使用的 worktree/构建产物。

## 5. 完整验收映射

| spec 验收 | 实现任务 | 最终证据任务 |
|---|---|---|
| A01 | T19、T25、T26 | T27 |
| A02 | T08–T10、T16、T19 | T27 |
| A03 | T17–T22、T25、T26 | T27 |
| A04 | T14、T15、T22 | T27 |
| A05 | T10、T11、T14–T16 | T27 |
| A06 | T05、T15、T22 | T27 |
| A07 | T01、T02、T17、T18 | T27 |
| A08 | T01、T02、T17、T19 | T27 |
| A09 | T03、T07、T08、T16、T26 | T27 |
| A10 | T11、T12、T14–T16、T20 | T27 |
| A11 | T11、T12、T15 | T27 |
| A12 | T12、T14、T16 | T27 |
| A13 | T12、T13、T23 | T27 |
| A14 | T09、T10、T13、T14 | T27 |
| A15 | T11、T18、T20、T22 | T27 |
| A16 | T07、T17、T20、T26 | T27 |
| A17 | T07、T12、T20 | T27 |
| A18 | T05、T14、T16、T23 | T27 |
| A19 | T12、T24 | T27 |
| A20 | T02、T17、T26 | T27 |

## 6. 计划复查与执行边界

编写后依次检查：spec 覆盖、前置依赖、接口/路径、失败与并发、测试真实性、发布安全、执行命令/工作目录。发现问题必须修正任务和矩阵，不只在复查记录里写“注意”。

### 本轮多轮复查记录

以下均为本会话的文档/源码交叉自查，不冒充独立人员评审或已运行产品测试。

1. 范围与依赖复查：将完整目标落实到 27 个主任务及 A01–A20 映射；保留 G0 和三份子协议/执行子计划的审阅门槛，明确大任务继续拆成单一行为红绿循环，禁止未经验证直接实现整套协议。
2. 真实入口复查：补齐原生 probe 构建、报告通道、无副作用 checker 导入、sidecar 和产物定位；补 CLI 的 `config.rs` 与独立 daemon 的 IPC/dispatcher 接线，旧协议或维护通知丢失不能放行。
3. 业务依赖闭包复查：补齐设置 adapter、错误映射、Popover、主题、Vitest setup 和 SDK 构建顺序；为第二插件增加真实原生 fixture 后端，避免 UI 验收请求不存在的程序。
4. 权限与事务复查：补齐首次激活/项目授权关系、Maintenance 不可由插件申请、旧 turn 收尾须核对真实 permit、结果查询与新准入分离、跨实例配置及项目所有权；补齐回退、权限变更、接管、清除与 GC 的管理入口。
5. 迁移与发布复查：补齐侧栏、重连、flags 和三个宿主契约依赖的清理；区分 probe 与无测试后门的正式发布物，超时杀进程不算恢复；正式签名/真实远端证据缺失则阻止相应验收，不自动发布。
6. 可执行性复查：统一 Rust 核心测试的绝对 manifest 路径和子 shell 工作目录，固定 Node schema 依赖解析、SDK exports 和各 workspace 验证；跨请求语义用 trace validator，不以单报文 schema 冒充行为测试。

文档检查覆盖任务唯一性、每任务五步骤/文件/接口/提交标题、依赖无环、A01–A20 完整映射、链接/围栏/占位符/尾空白及进度 18/19。本需求没有修改主程序或插件实现；Rust 格式检查通过。初稿的 `just audit` 曾因 B0 的 zip 漏洞失败，当时未提交/合并；本次文档交付同步纳入 main 的独立修复，并在提交前及合并结果上重新执行完整门禁，实际结果以交付时日志为准。

收敛标准：重复终查未发现新的文档遗漏、依赖环或与 spec 冲突的已知问题；这不等于已证明 WebView 隔离可行，也不等于产品已通过安全/平台验收。

技术验证和子 spec 审阅通过后才能执行依赖任务。计划编写不代表实现完成；用户后续要求入队后可按本文门槛推进技术验证和子协议工作，但不得跳过 G0–G3，也不包含生产签名或发布授权。任何审计失败都不得通过减少 CI 项目或加入无依据豁免解决。
