# 分级测试系统设计:复杂 one-shot 任务测试

Date: 2026-07-26

## 背景

现有真实 LLM 测试系统分两层:

- **Tier 1**:provider 层冒烟(`yi-agent-llm/tests/real_integration.rs`)— SSE 解析、鉴权
- **Tier 2**:简单端到端(`yi-agent/tests/e2e_real.rs`)— 单轮文本回复、单次工具调用

这些测试验证了"管道通畅",但**没有验证 agent 能否完成真实的多步骤生成任务**。例如
"生成一个个人网站"这种 one-shot 任务,需要 agent 一次性完成"理解需求 → 规划 → 多次
工具调用(write/bash)→ 产出完整产物",任何中间环节断裂都会导致产物缺失或结构错误。

现有 Tier 2 测试只检查"是否调用了 read/bash 工具",不检查"产出的文件是否结构完整"。

## 目标

1. **新增 Tier 3:复杂 one-shot 任务测试** — 验证 agent 能完成多步骤生成任务
2. **结构化断言** — 对产出文件做结构性验证(存在、大小、关键标记),不执行产出代码
3. **隔离性** — 每个测试独立 `TempDir`,互不污染,自动清理
4. **复用现有基础设施** — `yi-agent run --json` CLI + JSONL 解析,不新建执行入口
5. **不破坏现有测试** — Tier 1/2 代码不变,提取共享 helper 供 Tier 3 复用

## 设计决策

| 决策点 | 选择 | 理由 |
|---|---|---|
| 分层结构 | 4 层(mock / smoke / e2e / complex) | 逐层递进,granularity 清晰 |
| 复杂测试数量 | 4 个场景 | 覆盖典型生成任务类型,不过度膨胀 |
| 断言深度 | 结构性断言(文件存在/大小/标记) | 避免 model variance 导致 flaky,不执行产出代码 |
| 工作目录 | `tempfile::TempDir` per test | 全隔离,自动清理,无运行间污染 |
| 超时 | 300s per test | 复杂任务需要多次工具调用,给足空间 |
| 模型配置 | 默认模型(`.env` 中的 key) | 不引入额外配置,测试用户实际使用的模型 |
| max_turns | 默认 50 | 复杂任务典型 ~15 次工具调用,50 足够 |
| 测试文件 | 新建 `tests/e2e_complex.rs` | 与 `e2e_real.rs` 分离,职责清晰 |
| Helper 共享 | 提取到 `tests/common/mod.rs` | 避免 `e2e_real.rs` 与 `e2e_complex.rs` 重复 |
| Prompt 存储 | 内联在测试文件中 | 自包含,无需额外 fixture 文件 |
| Justfile | 新增 `test-real-complex`,更新 `test-real-all` | 现有 recipe 不变 |

## 架构

### 1. 分层结构

| Tier | 名称 | 文件 | Gate | 目的 |
|---|---|---|---|---|
| 0 | Mock | 各 crate `tests/*.rs`(非 `#[ignore]`) | 总是跑 | wiremock 模拟,无 API key |
| 1 | Provider smoke | `yi-agent-llm/tests/real_integration.rs` | `#[ignore]` + env key | SSE 解析、鉴权(**不变**) |
| 2 | Simple e2e | `yi-agent/tests/e2e_real.rs` | `#[ignore]` + env key | 单轮文本、单工具调用(**逻辑不变**,helper 提取) |
| 3 | Complex one-shot | `yi-agent/tests/e2e_complex.rs`(**新建**) | `#[ignore]` + env key | 多步骤生成任务 |

### 2. 测试场景

4 个复杂 one-shot 场景,每个验证 agent 完成多步骤生成任务的能力:

#### 场景 1:个人网站生成

```
Prompt: "Create a single-page personal website. Write the complete HTML
(with inline CSS) to output/index.html. The page should include a header,
an 'About' section, and a footer. Use the write tool to create the file."
```

Setup:`tempdir/output/`(空目录)

断言:
- `output/index.html` 存在
- 文件大小 > 500 bytes
- 内容包含 `<html`(case-insensitive)
- 内容包含 `<body`
- 内容包含 `About`
- 内容包含 `<footer`

#### 场景 2:Python 工具脚本

```
Prompt: "Write a Python function called `sort_list` that takes a list
and returns it sorted in ascending order. Write it to output/sort.py.
The file should be a valid Python module with a `if __name__ == '__main__'`
guard that demonstrates the function."
```

Setup:`tempdir/output/`(空目录)

断言(纯结构性,不执行 Python):
- `output/sort.py` 存在
- 文件大小 > 100 bytes
- 内容包含 `def sort_list`
- 内容包含 `__main__`

#### 场景 3:数据转换

```
Prompt: "Read the file input/data.json, extract all `name` fields,
convert them to uppercase, and write the result as a JSON array to
output/results.json."
```

Setup:
- `tempdir/input/data.json`:
  ```json
  [{"name":"alice","age":30},{"name":"bob","age":25},{"name":"charlie","age":35}]
  ```
- `tempdir/output/`(空目录)

断言:
- `output/results.json` 存在
- 内容是合法 JSON(serde_json::from_str 成功)
- 是 JSON 数组
- 数组长度 == 3
- 内容包含 `ALICE`、`BOB`、`CHARLIE`(case-insensitive substring)

#### 场景 4:Bug 修复

```
Prompt: "The file buggy.py contains a Python function with a bug.
Read it, identify the bug, fix it, and write the fixed version to
output/fixed.py. Do not just copy the original — fix the bug."
```

Setup:
- `tempdir/buggy.py`:
  ```python
  def add(a, b):
      return a - b   # BUG: should be +

  if __name__ == "__main__":
      print(add(2, 3))
  ```
- `tempdir/output/`(空目录)

断言:
- `output/fixed.py` 存在
- 文件大小 > 50 bytes
- 内容包含 `def add`
- 内容包含 `+`(修复后应含加法)
- 内容**不**包含 `return a - b`(原始 bug 行应被替换)

### 3. 执行架构

所有 Tier 3 测试共享以下流程:

```rust
#[tokio::test]
#[ignore]
async fn complex_personal_website() {
    let _key = skip_if_no_key();  // 无 key 时 eprintln + return

    let tmp = tempfile::TempDir::new().unwrap();
    // setup: 创建子目录、seed 输入文件

    let output = tokio::time::timeout(
        Duration::from_secs(300),
        run_agent_with_cwd(PROMPT, tmp.path()),
    ).await;

    assert!(output.is_ok(), "timed out after 300s");
    let jsonl = output.unwrap();
    let events = parse_events(&jsonl);
    assert!(has_done_event(&events), "no Done event");

    // 结构性断言
    let html = std::fs::read_to_string(tmp.path().join("output/index.html"))
        .expect("index.html not created");
    assert!(html.to_lowercase().contains("<html"));
    // ...
}
```

关键点:
- **子进程执行**:`std::process::Command::new(agent_binary())` 调 `yi-agent run --json`,
  与现有 e2e 测试一致 — 验证真实 CLI,不是 in-process agent
- **cwd = tempdir**:agent 的文件工具在 tempdir 内操作,对仓库无感知
- **300s 超时**:`tokio::time::timeout` 包裹子进程,超时即失败
- **JSONL 解析**:复用 `e2e_real.rs` 的 `parse_events` 逻辑,检查 `Done{EndTurn}` 事件

### 4. 共享 Helper 模块

新建 `yi-agent-rs/crates/yi-agent/tests/common/mod.rs`,从 `e2e_real.rs` 提取:

```rust
// tests/common/mod.rs
pub fn skip_if_no_key() { /* 检查 ANTHROPIC_API_KEY / OPENAI_API_KEY,无则 eprintln + return */ }
pub fn agent_binary() -> PathBuf { /* 定位 target/debug/yi-agent */ }
pub async fn run_agent(prompt: &str, cwd: &Path) -> String { /* spawn, 返回 stdout JSONL */ }
pub fn parse_events(jsonl: &str) -> Vec<Value> { /* JSONL → Vec<serde_json::Value> */ }
pub fn has_done_event(events: &[Value]) -> bool { /* 检查 Done{EndTurn} */ }
```

`e2e_real.rs` 和 `e2e_complex.rs` 均通过 `mod common;` 引入,消除重复。

`e2e_real.rs` 改动最小化:仅将 inline helper 移至 `common/mod.rs`,测试逻辑本身不变。

### 5. Justfile 变更

新增 recipe,现有 recipe 不变:

```makefile
# 新增:跑复杂 one-shot 任务测试
test-real-complex:
    #!/usr/bin/env bash
    set -e
    if [ -z "$ANTHROPIC_API_KEY" ] && [ -z "$OPENAI_API_KEY" ]; then
        echo "skip: no API key set"
        exit 0
    fi
    cargo test -p yi-agent --test e2e_complex -- --ignored

# 更新:跑所有真实 LLM 测试(Tier 1 + 2 + 3)
test-real-all: test-real-llm test-real-e2e test-real-complex
    @echo "All real LLM tests passed"
```

### 6. CI 不变

`just ci` 仍只跑 `fmt-check lint test build`,真实 LLM 测试(Tier 1/2/3)均不在 CI 跑。

## 文件清单

| 文件 | 操作 | 说明 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent/tests/e2e_complex.rs` | 新建 | 4 个复杂场景测试 |
| `yi-agent-rs/crates/yi-agent/tests/common/mod.rs` | 新建 | 共享 helper |
| `yi-agent-rs/crates/yi-agent/tests/e2e_real.rs` | 修改 | 提取 helper 到 common,测试逻辑不变 |
| `yi-agent-rs/justfile` | 修改 | 新增 `test-real-complex`,更新 `test-real-all` |
| `CLAUDE.md` | 修改 | 新增"分级测试系统"小节 |
| `docs/project-management/` | 修改 | 更新测试模块状态 |

## 非目标 (YAGNI)

- 不执行产出代码(不跑 `python3 sort.py`、不 headless 浏览器验证 HTML 渲染)
- 不做语义级断言(不检查 HTML 是否"好看"、sort 是否真的排序了)
- 不引入测试框架抽象(不用 `rstest`、不用 `test-case` crate)
- 不并行跑多个复杂测试(顺序执行,避免 API rate limit)
- 不做性能/延迟基准测试
- 不在 CI 跑复杂测试

## CLAUDE.md 补充

在现有 `## 真实 LLM 测试` 章节后追加 `## 分级测试系统` 小节。
