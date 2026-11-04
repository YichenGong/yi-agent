# Process Selector Public Contract Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 定位并消除 `process_read` / `process_kill` 公开必填签名与运行时二选一校验的冲突，保留合法单字段调用与既有拒绝规则。

**Architecture:** 先对故障实例、源端 schema、provider 请求和模型可见定义逐层取证，只在已定位的边界实施根因修复。共享 schema 与工具描述明确可选/二选一契约，工具层测试保留既有输入语义，真实 provider 对 wiremock 的请求证明传输边界正确；实际故障入口验收是整体完成门禁。

**Tech Stack:** Rust、serde_json、Tokio、tempfile、wiremock；现有 yi-agent-core / yi-agent-tools / yi-agent-llm workspace crates，无新增第三方依赖。

## Global Constraints

- Spec：[2026-11-03-process-selector-contract-design.md](../specs/2026-11-03-process-selector-contract-design.md)，基线 `b4567a1c`。实现前比较最新代码，不将此基线等同于运行实例版本。
- 本文件编写会话只负责文档评审、提交、合并与入队，不实现或部署。此限制不适用于后续看板实现会话：入队后按本计划执行 Task 1–6，实际实例重启、真实 API 调用和正式 app 替换仍须分别取得授权。
- 所有命令块从当前开发 worktree 根目录独立执行；每个 cargo 命令块显式进入 `yi-agent-rs` 并使用 `set -eu`，不能依赖上一任务遗留 cwd，也不能忽略前一条命令失败。
- 所有修改在 worktree 分支进行；不得在 main 直接写代码、文档或配置。
- 两个选择器各自可选，但必须且只能提供一个；未选字段省略，不用空字符串占位。
- `required: []` 为显式声明，不是已证明的根治措施。保留顶层 object，不引入顶层 oneOf / anyOf / allOf / not / enum / const。
- 保留 Option 反序列化、null 兼容行为及按字段存在性互斥；不将空字符串转成 None、不裁剪名称、不默选某字段。
- 不改变 cursor / max_bytes、输出分页、进程管理、权限或 kill 整组回收语义。
- 不盲目启用 strict；若实际入口不能支持公开可选契约，停止该适配分支并另行设计评审。
- 外部修复没有权限、关键 schema 无法取得或首个变形边界未定位时，不实施猜测性的转换层修改，不宣布整体修复完成。
- 真实实例重启、真实 API 调用、正式 app 替换须分别取得授权；默认 mock 测试不使用真实 API key。
- 实现提交前 `cargo fmt --all` + `just ci`；合并结果上再跑 `just ci`。不得为频繁小提交跳过项目门禁。

## 文件与职责

| 文件 | 责任 |
|---|---|
| `docs/research/2026-11-03-process-selector-contract-evidence.md`（新建） | 脱敏的实际链路定位、授权范围、故障重验和 C1–C7 结果 |
| `yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs` | 共享 schema/工具描述；源码合同与真实进程输入矩阵测试 |
| `yi-agent-rs/crates/yi-agent-tools/Cargo.toml` | 增加已有 workspace crate yi-agent-llm 的仅测试依赖 |
| `yi-agent-rs/Cargo.lock`（由 cargo 生成） | 同步本地 crate 测试依赖边；不手工改锁文件 |
| `yi-agent-rs/crates/yi-agent-tools/tests/process_selector_request_contract.rs`（新建） | 用真实 read/kill schema 调三条 provider 路径，捕获请求断言 |
| 实际首个变形边界对应文件或发布配置 | 由 Task 1 证据确定；未确定前不得自行指定/修改 |
| `docs/project-management/yi-agent-tools.md`、`README.md` | 依据验收记录维护状态与真实计数 |

不为定位增加常驻日志系统，不重构三个 provider，也不将测试 helper 加入生产接口。

## 验收映射

| Spec | 任务 |
|---|---|
| C1 源端 schema | Task 2 |
| C2 参数矩阵、C3 拒绝副作用 | Task 3 |
| C4 provider 请求 | Task 4 |
| C5 模型可见定义、C6 故障入口重验 | Task 1 + Task 5 |
| C7 生效/回归 | Task 5 + Task 6 |

---

### Task 1: 故障链路取证与根因修复门禁

**Files:**
- Create: `docs/research/2026-11-03-process-selector-contract-evidence.md`
- Read: `yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs`
- Read: `yi-agent-rs/crates/yi-agent-core/src/tool.rs`
- Read: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs`
- Read: `yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs`
- Read: `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/types.rs`

**Interfaces:**
- Consumes: 故障实例访问权限、会话公开工具签名、当前 spec。
- Produces: 证据文件中的“源端/请求端/模型可见端”三层对照，首个变形边界或明确未定位结论，允许修复的具体文件/组件与权限范围。

- [ ] Step 1: 按 using-git-worktrees 检查现有隔离；在后续开发 worktree 核对状态与基线。只搜索当前项目或用户明确提供的实例路径。命令中使用项目 `git grep`，不要假设 rg 位于 PATH。

```sh
git status --short
git branch --show-current
git rev-parse HEAD
git rev-parse --git-dir
git rev-parse --git-common-dir
git grep -n 'process_selector_schema' -- yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs
git show 2d441457 -- yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs
```

- [ ] Step 2: 在真实故障会话记录工具提供者身份及原始模型可见定义（read 与 kill）；通过已知实例路径/服务配置取得版本。若无法识别实例，先请求用户提供入口或二进制路径，不递归扫描系统目录。不要启动进程来猜测当前服务版本。
- [ ] Step 3: 在获授权的诊断入口导出运行实例源端 schema、实际选用 provider 请求里的 schema、最终公开签名。只保留工具名、description、properties、required、strict 及实例身份；不保存请求 headers、密钥、完整对话或进程输出。不能用仓库生成的 schema 冒充运行实例导出。
- [ ] Step 4: 写证据记录，逐项使用“已取得/无法取得 + 原因”而非空白，包含：

| 必须记录的字段 | 内容要求 |
|---|---|
| 实例身份 | 实际入口/路径、版本来源、与 checkout 的关系；不可确定则明确记为不可确定 |
| 源端定义 | read/kill 的 required、字段类型、说明；注明取得途径 |
| 请求端定义 | 实际 provider/API style 及对应 schema；仓库三条 provider 不一定是故障链路 |
| 模型可见定义 | 两字段可选标记、二选一说明及会话是否新建 |
| 首个变形边界 | 相邻两层定义差异、对应组件；无证据则写“尚未定位” |
| 实施授权 | 可修改范围、是否允许更新/重启实例、是否允许真实 API |
| C1–C7 | 每项使用通过/失败/未执行，加证据位置或阻塞原因；此时尚未验证的项必须为未执行 |

- [ ] Step 5: 作门禁判定。旧实例错误时选择更新对应源代码/版本；转换层错误时指向已确认的文件；全链路正确时检查旧会话持有定义的证据并准备新会话重验。若外部无权限、严格模式不兼容或边界尚未定位，在记录中声明根因修复阻塞。Task 2–4 可以独立建立源端合同与护栏，但不能越过 Task 5 的实际入口验收。

**Verification:** 人工逐层比对保留的脱敏定义；证据记录必须区分观察事实与推测。此任务不把本来正确的源码单字段行为当作故障复现，不需要人为制造线上双字段副作用。

### Task 2: 先红后绿地明确源端公开契约

**Files:**
- Modify/Test: `yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs`

**Interfaces:**
- Consumes: `ProcessReadTool::new(Arc<ProcessManager>)`、`ProcessKillTool::new(Arc<ProcessManager>)`、`Tool::schema() -> Value`、`Tool::description() -> &str`。
- Produces: 两工具显式 `required: []`、Optional 字段说明和 exactly-one 工具说明；不改 selector_from_args。

- [ ] Step 1: 在现有 tests 模块新增下面的合同测试。它捕捉对外描述与显式可选声明缺失，不是反复断言源码文本。

```rust
#[test]
fn process_selector_schema_contract() {
    let temp = TempDir::new().unwrap();
    let manager = ProcessManager::new(temp.path().to_path_buf());
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(ProcessReadTool::new(manager.clone())),
        Box::new(ProcessKillTool::new(manager)),
    ];
    for tool in tools {
        let schema = tool.schema();
        assert_eq!(schema["type"], "object", "{}", tool.name());
        assert_eq!(schema["required"], serde_json::json!([]), "{}", tool.name());
        assert!(tool.description().contains("exactly one"), "{}", tool.name());
        assert!(tool.description().contains("Omit"), "{}", tool.name());
        assert!(tool.description().contains("empty string"), "{}", tool.name());
        for (field, other) in [("process_id", "name"), ("name", "process_id")] {
            assert_eq!(schema["properties"][field]["type"], "string");
            let description = schema["properties"][field]["description"]
                .as_str().expect("field description");
            assert!(description.contains("Optional"), "{}.{field}", tool.name());
            assert!(description.contains("omit"), "{}.{field}", tool.name());
            assert!(description.contains(other), "{}.{field}", tool.name());
        }
        for key in ["oneOf", "anyOf", "allOf", "not", "enum", "const"] {
            assert!(schema.get(key).is_none(), "{}: {key}", tool.name());
        }
    }
}
```

- [ ] Step 2: 运行新测试，确认它因 required/公开说明缺失而失败，而非编译错误。若执行时最新代码已满足合同，记录已有修复和测试绿灯，不虚称红灯、不为制造红灯改生产语义。

```sh
set -eu
cd yi-agent-rs
cargo test -p yi-agent-tools --lib process::tools::tests::process_selector_schema_contract -- --exact
```

- [ ] Step 3: 仅修改两个 description 的返回字符串、共享属性说明和共享返回值的 required。保持额外 properties 合并与运行时实现不变。

```rust
// ProcessReadTool::description
"Read buffered stdout and stderr from a managed background process. Provide exactly one of process_id or name. Omit the other field; do not use an empty string as a placeholder."

// ProcessKillTool::description
"Kill a managed background process. Provide exactly one of process_id or name. Omit the other field; do not use an empty string as a placeholder."

// process_selector_schema 中两字段的 description
// process_id:
"Optional managed process id returned by process_start or process_list. Provide exactly one of process_id or name; omit name when using process_id."
// name:
"Optional unique managed process name. Provide exactly one of process_id or name; omit process_id when using name."

// 保留合并 extra_properties 的现有实现后返回：
serde_json::json!({
    "type": "object",
    "properties": properties,
    "required": []
})
```

- [ ] Step 4: 跑新旧 schema 合同及 process 工具测试。确认 read 的 cursor/max_bytes 属性未改变，metadata 权限未改变。

```sh
set -eu
cd yi-agent-rs
cargo test -p yi-agent-tools --lib process::tools::tests::
```

**Review gate:** 这是源端契约说明改进，只兑现 C1；不得声称已找到/修复公开定义变形。

### Task 3: 固定真实工具输入矩阵及拒绝副作用

**Files:**
- Modify/Test: `yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs`

**Interfaces:**
- Consumes: `Tool::call(Value) -> ToolResult`；`ProcessManager::list() -> Vec<ManagedProcessSnapshot>`；`ProcessManager::shutdown().await`；现有 `text(&ToolResult) -> &str`。
- Produces: `process_selector_argument_contract`；同一输出按 name/id 读取、独立实例按 name/id kill、非法输入不改变进程状态的护栏。

- [ ] Step 1: 在现有 tests 模块增加下面测试。长驻命令有固定终点，用 readiness 等待避免靠固定 sleep 推测输出完成；每个合法 kill selector 使用新的 manager/进程。

```rust
#[tokio::test]
async fn process_selector_argument_contract() {
    // 四种合法 kill 输入各用独立 manager/进程，不复用已终止实例。
    for kill_case in ["name", "id", "name-with-null", "id-with-null"] {
        let temp = TempDir::new().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        let start = ProcessStartTool::new(manager.clone());
        let read = ProcessReadTool::new(manager.clone());
        let kill = ProcessKillTool::new(manager.clone());
        let started = start.call(serde_json::json!({
            "command": "printf selector-ready; sleep 30",
            "name": "selector-contract",
            "ready_pattern": "selector-ready",
            "ready_timeout_sec": 5
        })).await;
        assert!(!started.is_error, "{}", text(&started));
        let started_json: Value = serde_json::from_str(text(&started)).unwrap();
        assert_eq!(started_json["ready"], true);
        let id = started_json["process_id"].as_str().unwrap().to_owned();

        let valid_reads = [
            serde_json::json!({"process_id": id}),
            serde_json::json!({"name": "selector-contract"}),
            serde_json::json!({"process_id": id, "name": null}),
            serde_json::json!({"name": "selector-contract", "process_id": null}),
        ];
        for args in valid_reads {
            let result = read.call(args).await;
            assert!(!result.is_error, "{}", text(&result));
            let output: Value = serde_json::from_str(text(&result)).unwrap();
            assert_eq!(output["process_id"], id);
            assert_eq!(output["stdout"], "selector-ready");
        }

        // want 是手工选择的错误类别；不从被测函数计算期望。
        let invalid = [
            (serde_json::json!({}), "exactly one"),
            (serde_json::json!({"process_id": null, "name": null}), "exactly one"),
            (serde_json::json!({"process_id": id, "name": "selector-contract"}), "exactly one"),
            (serde_json::json!({"process_id": "", "name": "selector-contract"}), "exactly one"),
            (serde_json::json!({"process_id": id, "name": ""}), "exactly one"),
            (serde_json::json!({"process_id": "", "name": ""}), "exactly one"),
            (serde_json::json!({"process_id": ""}), "process not found"),
            (serde_json::json!({"name": ""}), "process not found"),
        ];
        for (args, want) in invalid {
            let before = manager.list().into_iter().find(|p| p.process_id == id).unwrap();
            let read_result = read.call(args.clone()).await;
            assert!(read_result.is_error, "read accepted {args}");
            assert!(text(&read_result).contains(want), "{}", text(&read_result));
            let kill_result = kill.call(args.clone()).await;
            assert!(kill_result.is_error, "kill accepted {args}");
            assert!(text(&kill_result).contains(want), "{}", text(&kill_result));
            let after = manager.list().into_iter().find(|p| p.process_id == id).unwrap();
            assert_eq!(after.status, before.status, "invalid kill changed status: {args}");
            assert_eq!(after.pid, before.pid);
            assert_eq!(after.ready, before.ready);
            let still_readable = read.call(serde_json::json!({"process_id": id})).await;
            assert!(!still_readable.is_error, "{}", text(&still_readable));
            let output: Value = serde_json::from_str(text(&still_readable)).unwrap();
            assert_eq!(output["stdout"], "selector-ready");
        }

        let kill_args = match kill_case {
            "name" => serde_json::json!({"name": "selector-contract"}),
            "id" => serde_json::json!({"process_id": id}),
            "name-with-null" => serde_json::json!({"name": "selector-contract", "process_id": null}),
            "id-with-null" => serde_json::json!({"process_id": id, "name": null}),
            _ => unreachable!(),
        };
        let killed = kill.call(kill_args).await;
        assert!(!killed.is_error, "{}", text(&killed));
        let snapshot = manager.list().into_iter().find(|p| p.process_id == id).unwrap();
        assert_eq!(snapshot.status, crate::process::manager::ProcessStatus::Killed);
        manager.shutdown().await.unwrap();
    }
}
```

- [ ] Step 2: 运行输入矩阵测试。这是已有正确行为的特征测试，基线应为 PASS；若失败，使用 systematic-debugging 查明是否存在额外 bug，再评审是否属于当前修复范围，不为遵守 TDD 擅改选择器规则。

```sh
set -eu
cd yi-agent-rs
cargo test -p yi-agent-tools --lib process::tools::tests::process_selector_argument_contract -- --exact
cargo test -p yi-agent-tools --lib process::
```

- [ ] Step 3: 审查测试能否捕捉目标变更：如果 selector_from_args 将两个 Some 默选其一，双字段错误与状态不变断言应失败；如果空串被转为 None，空字符串占位用例应失败；如果 null 不再兼容，valid_reads/合法 kill 应失败。需要突变验证时只在隔离工作区临时变更，验证失败后立即恢复，不提交突变。

**Review gate:** 既有 ProcessManager 整组 kill 测试承担真实子孙回收；本任务不重复发明进程探针。测试 panic 时 manager 的既有 Drop 负责兜底，成功路径显式 shutdown；不得留下无限长 busy loop。

### Task 4: 真实 schema 穿过三条 provider 的 HTTP 请求护栏

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/Cargo.toml`（仅 dev-dependencies）
- Generated if needed: `yi-agent-rs/Cargo.lock`
- Create/Test: `yi-agent-rs/crates/yi-agent-tools/tests/process_selector_request_contract.rs`

**Interfaces:**
- Consumes: `ToolRegistry::register(Arc<dyn Tool>)`、`ToolRegistry::schemas() -> Vec<ToolSchema>`、三条 provider 的公开构造器及 `Provider::call(ProviderRequest)`。
- Produces: `process_selector_request_contract`，覆盖真实 HTTP POST 的 read/kill schema，不新造固定 input_schema fixture、不修改 llm 生产代码。

- [ ] Step 1: 在 tools crate 的 dev-dependencies 中加入现有 workspace crate，不新增外部库：

```toml
yi-agent-llm = { workspace = true }
```

确认 workspace 已有此成员引用，并按项目规则核对 cargo metadata 的依赖 rust-version；没有外部依赖版本升级时不盲目修改 MSRV。此测试位置选择 tools crate 是为了取得真实工具 schema，避免把 FS 工具测试依赖引入 llm crate。

- [ ] Step 2: 新建完整集成测试，显式选择 chat/responses 避免 auto 探测额外请求。只匹配 method/path 并捕获请求体，不能靠 body matcher 返回成功冒充测试断言。

```rust
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use yi_agent_core::{GenParams, Message, Provider, ProviderRequest, ToolRegistry};
use yi_agent_llm::{AnthropicProvider, AnthropicProviderOpts, OpenaiProvider, OpenaiProviderOpts};
use yi_agent_tools::process::manager::ProcessManager;
use yi_agent_tools::process::tools::{ProcessKillTool, ProcessReadTool};

#[tokio::test]
async fn process_selector_request_contract() {
    for style in ["anthropic", "chat", "responses"] {
        let server = MockServer::start().await;
        let temp = tempfile::TempDir::new().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(ProcessReadTool::new(manager.clone())));
        registry.register(std::sync::Arc::new(ProcessKillTool::new(manager)));
        let (request_path, sse) = match style {
            "anthropic" => (
                "/v1/messages",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ),
            "chat" => (
                "/v1/chat/completions",
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            ),
            "responses" => (
                "/v1/responses",
                "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n",
            ),
            _ => unreachable!(),
        };
        Mock::given(method("POST"))
            .and(path(request_path))
            .respond_with(ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse))
            .expect(1)
            .mount(&server).await;
        let provider: Box<dyn Provider> = if style == "anthropic" {
            Box::new(AnthropicProvider::new(AnthropicProviderOpts {
                base_url: Some(server.uri()),
                api_key: Some("test-key".into()),
                timeout: Some(Duration::from_secs(5)),
                ..Default::default()
            }).unwrap())
        } else {
            Box::new(OpenaiProvider::new(OpenaiProviderOpts {
                base_url: Some(format!("{}/v1", server.uri())),
                api_key: Some("test-key".into()),
                timeout: Some(Duration::from_secs(5)),
                api_style: Some(style.into()),
                ..Default::default()
            }).unwrap())
        };
        provider.call(ProviderRequest {
            model: "test-model".into(),
            system: None,
            messages: vec![Message::user("check tool schemas")],
            tools: registry.schemas(),
            params: GenParams::default(),
        }).await.expect("mock provider call");
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1, "{style}: unexpected probe/retry");
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        let tools = body["tools"].as_array().expect("transmitted tools");
        assert_eq!(tools.len(), 2, "{style}");
        for name in ["process_read", "process_kill"] {
            let tool = tools.iter().find(|tool| {
                let definition = if style == "chat" { &tool["function"] } else { tool };
                definition["name"] == name
            }).expect("both tools transmitted");
            let definition = if style == "chat" { &tool["function"] } else { tool };
            let schema = if style == "anthropic" {
                &definition["input_schema"]
            } else {
                &definition["parameters"]
            };
            assert_eq!(schema["type"], "object", "{style}/{name}");
            assert_eq!(schema["required"], serde_json::json!([]), "{style}/{name}");
            assert_ne!(definition["strict"], true, "strict unexpectedly enabled");
            let description = definition["description"].as_str().unwrap();
            assert!(description.contains("exactly one"));
            assert!(description.contains("Omit"));
            assert!(description.contains("empty string"));
            for (field, other) in [("process_id", "name"), ("name", "process_id")] {
                assert_eq!(schema["properties"][field]["type"], "string");
                let description = schema["properties"][field]["description"].as_str().unwrap();
                assert!(description.contains("Optional"));
                assert!(description.contains("omit"));
                assert!(description.contains(other));
            }
            for key in ["oneOf", "anyOf", "allOf", "not", "enum", "const"] {
                assert!(schema.get(key).is_none(), "{style}/{name}: {key}");
            }
        }
    }
}
```

- [ ] Step 3: 运行测试。若先于 Task 2 运行，缺席 required/说明会使它红；在 Task 2 后运行则应直接绿，这是传输护栏，不说明变形发生在 provider。若断言失败，按捕获请求定位具体代码，再将修复纳入 Task 1 证据链，不能无条件重写 provider。

```sh
set -eu
cd yi-agent-rs
cargo test -p yi-agent-tools --test process_selector_request_contract
cargo test -p yi-agent-llm
```

- [ ] Step 4: 审查反例：把任一 provider 输出临时改成 `required: ["name","process_id"]`，该路径请求断言应失败；若需实跑突变，仅在隔离 worktree 临时操作并立即恢复。三条路径必须都出现一次实际 POST；不能跳过某条路径却把 C4 标为通过。

### Task 5: 在已定位边界根治并完成故障入口验收

**Files:**
- Modify: `docs/research/2026-11-03-process-selector-contract-evidence.md`
- Modify/Test: Task 1 证据指向的实际组件；无确定路径或无权限时不修改。

**Interfaces:**
- Consumes: Task 1 的组件/授权判定；Task 2–4 的源端合同和测试；真实故障入口。
- Produces: 可复查的根因修复 red/green 或版本更新证据，C5/C6 通过记录及准确生效步骤；无法完成则交付明确阻塞。

- [ ] Step 1: 再次核对 Task 1 的边界与变更权限。若修改属于本仓库，根据已定位文件写一个最小边界回归：同一 read/kill 输入 schema 无单独必填字段，实际转换输出两个字段仍为可选；错误旧输出应使测试失败。固定输入输出不是由同一个 builder 推导期望值。边界尚未定位时不能预先提供或执行猜测的转换代码。
- [ ] Step 2: 依据证据选择且只执行相应分支：
  - **旧版本实例**：确认正确构建版本和当前版本不同，按已有发布流程准备授权范围内的隔离实例；记录旧/新版本、源端 schema 与模型可见定义。若无需改代码，不制造无意义的源码修复。
  - **本仓库转换边界**：先让边界回归在旧逻辑失败，再只修改错误 required/可选标记映射，使省略字段保持省略；空 required 与缺席 required 都不得推导成全部必填。测试通过后重复三层对照。
  - **仓库外转换边界**：向有权限的维护者交付脱敏输入、错误输出、期望输出和回归步骤。只有取得外部修复及实际入口重验结果才能继续；否则保留阻塞。
  - **旧会话定义**：只有版本/定义证据证明会话刷新是生效条件时，按授权创建新会话并比较，不断言重启一定解决。
  - **入口不能满足可选契约**：记录具体严格模式限制，提出适配设计评审；不将 nullable 必填字段或空串归一化作为本计划的静默替代。

此任务有意不写未知转换层的伪代码：确定组件后的具体回归/最小补丁需补充到证据记录并过评审，未满足门禁即停止该分支，不是留空实现任务。

- [ ] Step 3: 在已授权的真实入口/新会话确认 read 与 kill 的模型可见 `name?` / `process_id?` 或等价可选表示，并确认二选一说明。不要只检查源码 `schema()`。
- [ ] Step 4: 创建只用于验收的命名托管进程，取得实际返回 id；通过真实公开工具入口依次仅传 name、仅传 id 读取输出；两种都不能被入口要求补另一个字符串。使用下列输入形状，名称/id 必须由本次验收实例实际产生，不复用历史例子。

```json
{"name":"selector-acceptance"}
```

read 的 id-only 输入只含本次返回的 process_id。双有效字段 read 应返回互斥错误；空字符串占位双字段也应拒绝。对 kill 验证两个单选择器定义可调用，并在各自新建的测试实例上分别验收，不碰用户其他托管进程。

- [ ] Step 5: 记录实际结果、时间、实例版本、三层定义和生效步骤。清理只由本次创建的测试进程；清理失败记录具体残留身份，不通过模糊名称或批量 pkill 清其他任务。没有实际入口访问时 C5/C6 必须为未执行/阻塞。

### Task 6: 回归、进度与交付门禁

**Files:**
- Modify: `docs/project-management/yi-agent-tools.md`
- Modify: `docs/project-management/README.md`
- Modify: `docs/research/2026-11-03-process-selector-contract-evidence.md`

**Interfaces:**
- Consumes: Task 1–5 证据及测试；C1–C7 状态。
- Produces: 完成或阻塞的准确进度、通过的项目门禁、可审查提交；按后续授权才合并。

- [ ] Step 1: 在 worktree 执行下列回归和唯一 CI 入口。预留 workspace 全量编译时间，使用托管进程持续读取日志；不要把排队等待 cargo 锁误判死锁，不共用其他 worktree target，不复制整个巨大 target 来省事。

```sh
set -eu
cd yi-agent-rs
cargo fmt --all
cargo test -p yi-agent-tools --lib process::
cargo test -p yi-agent-tools --test process_selector_request_contract
cargo test -p yi-agent-llm
just ci
```

预期：全部 exit 0；若非本次相关失败也必须记录并停止提交，不能跳过门禁后宣布全绿。真实 API 测试不加 `--ignored`。

- [ ] Step 2: 审查 C1–C7 对照记录。只有全通过才将模块条目改为 `[x]`；尚未定位、外部阻塞或无法实际入口重验则保持 `[ ]`，即使 Task 2–4 测试绿灯也不能宣称问题已修复。把完成判据指向现存测试及证据文件。
- [ ] Step 3: 更新 README 模块计数，按模块顶层 `- [x]` / `- [ ]` / `- [-]` 条目计算，不把 spec/plan 的步骤计入 feature；对齐索引，不复制旧计数。
- [ ] Step 4: 用 `git diff --check` 审查只包含必要变更，确认无密钥/未脱敏原始请求、无突变测试残留。提交前再次 `cargo fmt --all && just ci`，用 conventional commits、无 Co-Authored-By 行。可将 Task 2–4 作为一个可独立审查的合同与护栏提交，Task 5 根因修复另成一个提交；每个实现提交均须过项目要求的完整门禁。
- [ ] Step 5: 用 requesting-code-review 审查已定位的根因修复与拒绝副作用，不将说明改进作为根治。在看板实现会话中，计划完成后通过 `superpowers-kanban state --self awaiting_merge` 等待用户验收；有定位/授权阻塞则 `state --self needs_you --detail "实际阻塞原因"`，不能因部分护栏通过而申请整体合并。用户确认合并后使用 `superpowers-kanban merge-request <card-id>`，由 daemon 持有名额的独立合并回合执行 `git merge --no-ff` 并在合并结果运行 `just ci`；实现会话不得自行改 main，也不得自报 done。非看板的普通分支才按 finishing-a-development-branch 在合并授权后手工合并。看板 CLI 从项目主根执行或显式指定主根 state-dir，不在实现 worktree 裸跑。
- [ ] Step 6: 本轮开发已完成且不再需要增量编译时按项目磁盘规范先检查本次 worktree target 与编译进程，再清理本次已合并 worktree/分支；不删除其他开发 session 的产物。

## Plan 自检记录与交接

- Spec C1–C7 均有任务映射，原始未知根因被保留为证据门禁，没有被凭空替换成 required 空列表修复。
- schema 测试有明确红灯原因；运行时矩阵与 provider 转发以已有行为护栏处理，不虚构它们的基线失败。
- 三条 provider 请求由真实 ToolRegistry schema 产生，模型可见端另行验收，不将 wiremock 当作线上根治证据。
- 后续看板实现会话先完成 Task 1，按定位门禁决定实施或交付阻塞；完成后按 Task 6 的看板名额流程等待验收/合并。本文件编写会话只合并文档并入队，不直接执行开发任务、启动或替换正式实例；项目 CI 所需的隔离测试实例不属于部署。
