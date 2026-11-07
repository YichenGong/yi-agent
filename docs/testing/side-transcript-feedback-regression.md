# Desktop `/side` 正文空白：调查与回归记录

## 用户症状与验收边界

进入 `/side` 后，自己发送的问题与模型回复均不可见。以下证据来自隔离工作区、临时会话及本地 HTTP 模型，不使用用户的真实会话或 API key。

真实 CLI app-server → 桌面 React App → Chrome 链路与隔离 Desktop 原生 WKWebView 均已验收。原生验收使用真实 Tauri bridge 与 React 输入框，不伪造旁支通知；这不代表正在运行的正式 App 已更新。尚未替换或重启用户正式 App。

## 已证实的缺陷

1. `yi-agent-app-server/src/side_conversation/registry.rs::turn_start` 未调用 `SideTranscript::begin_turn`。runner 的私有 Session 有用户问题，但桌面读取的共享 transcript 没有。
2. `yi-agent-runtime/src/side_conversation/runner.rs::Pump::run` 收到 `AgentEvent::Error` 后结束事件流，却没有将错误存入 `Completion`。`wait_quiescent` 因此把失败轮次标为 Idle；宿主广播中没有 failed/error 反馈。
3. `yi-agent-app-server/src/side_conversation/source.rs::read_cold_parent` 只把 `meta.model` 显示串覆盖到基础配置，没有解析模型清单。隔离链路中父会话正常访问本地 OpenAI，旁支却沿用基础 Anthropic 配置，没有请求本地模型。应与 `server/runtime_binding.rs` 的 production factory 一致：会话 `model_ref` → 显式注入配置 → 当前清单默认，完整解析 provider/URL/model/style 等参数，并保留会话 effort 覆盖。

这些代码缺陷均有回归证据，但没有证据证明它们是用户当时实际环境中的唯一根因。

## 补丁范围

- 问题写入共享 transcript，并在模型输出发布前广播 `side/itemUpdated`。
- ledger 与 transcript 先生成候选状态并校验 wire item，runner 启动成功后才一起提交；拒收不能留下已受理请求、部分问题或未收尾任务。
- runner 保存 `AgentEvent::Error` 的失败终态。预算错误映射 `resource_limit`，其他调用错误映射 `turn_failed`，仅向客户端发布安全摘要，不带供应商原始响应正文。
- 每次旁支解析读取当前模型清单；测试显式注入清单，不依赖真实用户配置。
- 使用本地可控 provider 验证真实 runner、registry、Broadcaster 与 snapshot，不调用真实 LLM API。

## RED/GREEN 证据

| 回归 | RED | GREEN |
|---|---|---|
| `side_accepted_question_is_in_snapshot_and_published_once` | accepted question 未出现在 snapshot | 问题出现在 snapshot 且仅发布一次 |
| `side_provider_failure_is_failed_with_a_safe_error` | 实际 Idle，预期 Failed | Failed 且只有安全错误摘要 |
| registry provider 集成 | 错误 completed.status 实际 idle | 成功回答广播/snapshot 与失败 completed 均通过 |
| `side_transcript_refusal_does_not_start_or_accept_a_turn` | 拒收后仍留下 running turn | 拒收后无 live runner、无 dedup 受理记录、无部分 transcript |
| `side_cold_config_resolves_the_catalog_instead_of_the_display_model` | provider 实际 anthropic，预期 openai | 清单默认、会话选择、注入配置、effort 优先级均通过 |

## 已执行结果

| 检查 | 结果与边界 |
|---|---|
| runtime `side_` 定向回归 | 90 passed / 0 failed |
| 最新 app-server `side_` 定向回归 | 105 passed / 0 failed，含模型清单修复 |
| desktop `App.test.tsx -t 'App 旁支视图'` | 21 passed / 254 skipped |
| desktop sideConversation、sideSync、useSideConversation、useSideConversationController | 110 passed，4 files |
| `cargo fmt --all`、`git diff --check` | 已执行，无格式错误 |
| 最新 `cargo build -p yi-agent` | 成功；浏览器验收使用该真实 CLI 的 `app-server --listen stdio://` |
| 两个改动 crate 的 clippy | 早轮完成；模型清单修复后完整 CI 的全工作区 `--all-targets --all-features` lint 再次完成 |
| 独立静态审查：registry/runner | 无桌面路径阻断项；下节记录非阻断风险。没有由审查者执行测试 |
| 独立静态审查：模型清单修复 | 无桌面路径阻断项；模型选择与 effort 优先级一致，未发现凭据泄漏；未执行构建或测试 |

既有 ts-rs 属性解析、html5ever future incompat 警告仍存在，不能把它们表述为本次新引入。

### 真实浏览器链路

隔离 HOME/YI_AGENT_HOME、独立 Chrome profile、loopback 本地 OpenAI SSE；Tauri IPC shim 仅转发 RPC 到真实 app-server，不伪造旁支通知。

最终一次验收退出码 0，四项均通过：

1. `LOCAL_SIDE_QUESTION` 出现在旁支 transcript 的可见区域。
2. SSE 首段回答在 `side/turnCompleted` 之前已可见。
3. 流结束后第二段回答仍可见。
4. 本地 provider 返回含私有 marker 的 HTTP 400 后，界面显示安全中文失败提示，DOM 与 RPC 通知均不含原始私有正文。

本地模型共收到 4 次调用（父会话标题/回答、旁支成功/失败）；旁支问题实际访问 `/v1/chat/completions`。临时脚本、截图和未使用的最小 server 入口移至 `/tmp/yi-side-probe.OYDN8o/evidence/`，不提交硬编码路径或临时二进制。临时证据可被系统清理，持久回归依据为仓库中的自动测试及上述验证步骤。

### 原生 Desktop WKWebView 链路

在同步 `main` 的 `a0c9fe1e` 后重新构建真实 sidecar、前端 dist 与隔离 Tauri App，构建退出码均为 0。测试 bundle ID 为 `com.gongyichen.desktop.side-fix`；使用临时 HOME/YI_AGENT_HOME、临时项目和 loopback OpenAI SSE，不配置真实 key 或 relay，不操作 `/Applications/yi-agent.app`。

由于宿主未授予辅助功能权限，临时外置 dylib 在测试 App 的 WKWebView 创建时注入 WKUserScript，驱动真实 React 输入框与 Tauri RPC/event bridge。通知仍来自真实 sidecar，未使用 IPC shim。dylib 在初始化后清除注入环境变量，不注入 sidecar，也不进入仓库或产品包。保留产品 CSP、正常权限机制与三层临时签名。

最终独立运行退出码为 0，实际引擎 `AppleWebKit/605.1.15`，`nativeBridge=true`：

- `PASS_QUESTION`：旁支问题已可见。
- `PASS_STREAM_BEFORE_COMPLETION`：首段流式回答在完成通知前可见。
- `PASS_COMPLETED_ANSWER`：第二段和最终回答已可见。
- `PASS_SAFE_FAILURE`：HTTP 400 后显示安全中文提示，DOM/通知不含私有响应 marker。
- `PASS_ALL`、`SNAPSHOT_SAVED`：原生截图中可见问题、两段回答、错误提示及 `/side` 状态。

本地模型收到四次 `/v1/chat/completions` 请求；只在验收结束后终止该临时 App。最终临时证据为 `events.jsonl`、`native-side.png`、`desktop-stderr.log`（位于本机 `/private/tmp/yi-side-native-6t7znQ/`），可被系统清理，不作为持久自动测试的替代品。此前失败轮次是探针使用 `/tmp` 软链接路径、等待空会话禁用输入框或未捕获 App 退出的问题，不计为通过；最终成功轮在 CI 结束后独立运行。

同步 main 后前端全套 `npm test`：172 files / 2390 tests passed / 0 failed。第三次独立静态审查针对 `a0c9fe1e` 上的完整补丁，未发现阻断项；审查者未执行测试。

### 完整 CI 历史（不可混同最终门禁）

- 首轮系统加载长时间等待后主动停止，不构成全绿。
- 后一轮 nextest 编译中 `darling` rustc 被 SIGKILL，退出 137；未查明 SIGKILL 来源，不能断言 OOM。
- 同步 main 前的两次完整 `just ci` 分别为 5632/5633 tests run，各有一项 supervisor 静默桥接用例失败：客户端 2 秒 deadline 与 bridge 2 秒预算竞态，期望 I/O Err，却收到合法显式 Rejected。单项复验通过不能替代全量门禁。
- 后续同步最新 main `a0c9fe1e`，其中已经修正该 supervisor 测试，接受有界 I/O Err 或 Rejected/Unsupported，保持拒绝授权及 6 秒上界。本补丁没有修改 supervisor 或放宽预算。
- 同步后的第一轮完整 CI：5913 run / 5847 passed / 66 failed / 40 skipped。插件测试二进制内嵌已删除基线 worktree 的 `CARGO_MANIFEST_DIR`，经 `strings` 证实；定向 `cargo clean -p yi-agent-plugins` 清除旧构建产物，没有改 fixture、签名或测试代码。
- 清理后的完整 CI：5913 run / 5911 passed / 2 failed / 40 skipped。失败是插件官方源测试的本地 Node 服务器未输出端口，以及 executor 子进程组清理预算耗尽。两项随后以 `--test-threads 1` 单独 nextest 复验均通过（2/2）；这只证明单项复验，不证明完整 CI 已通过。
- 最终门禁使用 `NEXTEST_TEST_THREADS=2 CARGO_BUILD_JOBS=2 just ci`，保留所有测试、原 `ci` profile 和原超时：5913/5913 nextest 通过，40 项按既有 ignore gate 跳过；build、bindings、各脚本门禁、MSRV 1.89、32 项 eval 基线均通过，最终 `CI passed` / 退出 0。eval 二进制曾在 `_dyld_start` 启动阶段等待，采样占用 96 KB 且未进入测试入口；其磁盘签名有效，随后原 CI 正常继续完成，同一二进制独立复验也 32/32 通过，没有关闭系统签名机制。
- 两道依赖审计因网络更新 advisory-db 失败，按仓库既有 fallback 使用本地缓存并给出 stale 警告；`chacha20 0.10.1` 的 yanked 警告由既有策略允许。本轮未修改审计策略，缓存结果不代表检查了最新披露漏洞。
- 合并后的完整 `just ci` 将在 main 上再次运行，结果另行记录。

## 审查记录的非阻断后续风险（本轮未修改）

- TUI `side_driver.rs` 仍有 ledger/start/begin_turn 的相似原子性风险，不属于本次 desktop 路径。
- 失败后立即重试时，旧错误提示可能保留到下一轮完成。
- runner 下游事件队列满后停止转发的既有行为，在关键通知等待期间仍有风险；这不由本轮正常吞吐链路验收排除。
- 旁支每次直接读取清单，父会话使用保留最后有效值的 `CatalogWatcher`；文件瞬时损坏或不可读时两者可能采用不同回退配置。生产热路径辅助函数仍要求调用方先解析有效配置；空清单与悬空 `model_ref` 的冷读回退尚未新增专门用例。

## 交付边界

- 修复 worktree 的完整 `just ci` 退出 0，三次独立静态审查无阻断项，原生 WKWebView 四项验收通过。
- 合并结果必须再执行完整 `just ci`。
- 正式 App 尚未替换或重启，不能把代码完成表述为正在运行的旧 App 已更新。

## 复验命令

在修复 worktree 的 `yi-agent-rs/` 中：

```sh
cargo fmt --all
cargo test -p yi-agent-runtime --lib side_
cargo test -p yi-agent-app-server --lib side_
cargo clippy -p yi-agent-runtime -p yi-agent-app-server --all-targets -- -D warnings
just ci
```

全绿后才能按仓库 worktree 合并流程交付；正式实例部署另行确认，不得把现有 App 中的会话当作测试 fixture。
