# 质量加固第二波：变异测试、patch coverage、属性测试、golden-set eval、nightly

> 状态：设计待评审
> 日期：2026-11-02
> 前置：[2026-11-01-test-ci-hardening-design.md](./2026-11-01-test-ci-hardening-design.md)（已合入 main，见 `e2f87fad`）
> 本文承接前一份 Spec 的「附录 C：本设计未覆盖、需要后续处理的事项」，只处理其中第 1–5 项。

## 问题

第一波加固解决了「基础设施缺失」：nextest 迁移、JUnit 产出、桌面测试进 CI、macOS 门禁、若干脆弱测试的条件轮询化。但测试体系的**有效性**仍未被验证。本文的出发点是一个已经实测到的反例：

**行覆盖率 90.11%，变异分数只有 72.7%。**

2026-11-01 在本机（18 核 / 128 GB / macOS / Rust stable）实测：

```
cargo llvm-cov --workspace --lcov   →  165.7s，行覆盖 93868/104166 = 90.11%
cargo mutants --file permission.rs  →  123.7s，50 变异体：32 caught / 12 missed / 1 timeout
```

12 个存活变异体全部集中在 `crates/yi-agent-core/src/permission.rs`，且集中在同一类结构上：`tool_level_allows` 的五个 match arm 里，只有 `bash` 有测试覆盖。

实证：把这一行整个删掉——

```rust
"write" => config.tool_level.write,
```

代码照样编译，36 个 permission 测试照样全绿。因为该 arm 删除后落入 `_ => false`，而现有测试用的都是默认配置（`tool_level.write` 默认 `false`），两者结果相同。真实后果是：用户在配置里写 `tool_level.write = true`（期望"写文件不必再确认"），该设置**静默失效**，每次仍弹确认框。

**读写两侧的不对称性已实测确认**，这是根因的直接证据：写侧 `apply_decision` 的 `"write"` 分支（`permission.rs:298`）**被杀死**，因为 `:839` 有 `apply_decision("write", &Decision::AlwaysAllowTool, ...)` 测试；而读侧 `tool_level_allows` 的 `"write"` 分支（`:194`）**存活**，该行没有任何测试引用。同一个配置项，写入路径被测过、读取路径没测过——配置存得进去、读出来不生效。

这就是本文要解决的核心问题：**当前测试能证明"代码被执行过"，不能证明"代码行为正确"。**

## 目标

1. 建立变异测试作为「测试有效性」的度量，并为已知盲区设可执行的门禁
2. 建立 patch coverage：只卡 PR/合并新增行的覆盖，不设全仓总阈值
3. 用属性测试打三个解析不可信输入的纯函数
4. 建立 Agent golden-set eval，评**轨迹**而非只看结果
5. 建立 nightly 定时任务，覆盖真 LLM、变异测试、依赖审计

## 非目标

- **不修改** `permission.rs` 之外的权限逻辑设计；12 个存活体只补测试，不改行为
- **不引入** `quick-xml` / `zip` 的大版本升级（决策见「决策记录 D1」，单独开卡）
- **不追求** mutation score 达到某个全局百分比；只要求「已登记的三处不低于各自基线，且不得新增存活体」
- **不做** CI 分片、不做 JUnit 上传（前一份 Spec 附录 C 第 8、12 项，本波不处理）
- **不拆分**巨型测试文件（附录 C 第 7 项，另议）
- **不引入** insta 快照测试（附录 C 第 6 项，另议）

## 决策记录

本节记录 2026-11-02 与项目所有者逐项确认的决策。后续实施若与本节冲突，以实现目标准，但必须回头修订本节。

### D1：依赖漏洞处理 —— 选 C

`cargo audit` 实测 66.9s，报出 30 条（19 个 crate）。实测分三类：

| 类别 | crate | 可否修 | 处置 |
|---|---|---|---|
| 纯 lock 更新即可 | `quinn-proto` 0.11.16→0.11.19、`rustls` 0.23.42→0.23.46、`h2` 0.4.15→0.4.20、`rustls-webpki`、`webpki-roots`、`webpki-root-certs`、`thread_local`、`tokio`、`chacha20` | 是，`cargo update` 即可（`--dry-run` 已验证在更新范围内） | **本波执行** |
| 需改 Cargo.toml 约束 | `quick-xml`（现 `"0.38"`，2 个 high）、`zip` 8.6.0→9.0.0（3 个 high） | 是，但跨大版本，可能改 API | **单独开卡**，本波不做 |
| 无法修 | `regex 0.2.11`（来自 `html2md 0.1.1` 传递依赖）、`lru`/`bincode`/`paste`/`rand_os`/`ttf-parser`/`unsafe-libyaml`/`yaml-rust`/`ascii`（unmaintained 标记，非 CVE） | 否 | 录入豁免清单 |

**约束**：`cargo update` 会改动约 134 个包。执行时必须**等所有并行卡片合入 main 之后**单独做，不得与其他改动并行，否则 Cargo.lock 必冲突。

### D2：patch coverage 门禁位置 —— 选 A

成熟做法有三种落点：pre-push hook（强制）、CI 门禁、手动 recipe。选 **手动 recipe（`just ci-patch`）**，并在 worktree 收尾流程中要求执行。

理由：覆盖率必须用独立 `CARGO_TARGET_DIR`（实测 `target/llvm-cov-target` 占 7.2 GB）全量重编，耗时 165.7s 起。单人开发节奏下，塞进每次 push 必然被 `--no-verify` 绕过，最终等于没有。挂在「合并前收尾」这个唯一有意义的时点，配合既有的 `finishing-a-development-branch` 流程，遵守成本最低。

### D3：golden-set eval 评分维度 —— 选 B（结果 + 轨迹）

不选 A（只评结果）：现有 `e2e_complex.rs` 已是此模式，测不出「工具选错但碰巧把文件写出来」这类失败。
不选 C（B + 稳定性统计）：把单轮成本从 45 分钟推到 3 小时以上，而它要抓的非确定性在 B 阶段已能观察到苗头。

选 B 的技术前提已验证：`AgentEvent` 已含 `ToolCall` / `ToolResult` / `ToolExit` / `Usage`（`crates/yi-agent-core/src/agent.rs`），且 `tests/common/mod.rs` 的 `parse_events` 已能解析 `--json` JSONL。**做轨迹评估不需要修改 core。**

### D4：变异测试三处落点 —— 修正后确认

原清单写「权限判断 / bash 黑名单 / store 幂等」，实测后修正：

| 原计划 | 实测结论 | 最终落点 |
|---|---|---|
| 权限判断 | 与「bash 黑名单」是**同一文件** `permission.rs`（黑名单逻辑在 `check_blacklist_then_allow`，:168） | `crates/yi-agent-core/src/permission.rs` |
| bash 黑名单 | 同上，不构成独立落点 | 同上，合并为第 1 处 |
| store 幂等 | 真逻辑在 `store/src/runtime.rs:739` 的 `attach_application_root`（幂等键校验在 :769），但**该文件只有 10 个单测**；幂等性由 `tests/runtime_ipc.rs` 等集成测试保证。按「只打快单元测试」的约束，这条打不到 | **替换**为 `crates/yi-agent-tools/src/fs/path_util.rs`（文件路径边界，459 行 / 21 单测，纯函数） |
| （空出的第 3 处） | — | `crates/yi-agent-llm/src/sse.rs`（SSE 流解析，267 行 / 10 单测，外部不可信输入；入口为 `SseLineParser` / `SseStreamCore`，无自由函数） |

三处形成完整闭环：**工具权限** → **文件路径边界** → **外部数据解析**，覆盖三种不同的攻击面。

**实测补充（2026-11-02）**：`sse.rs` 首轮变异测试为 11 变异体 / 4 caught / 7 unviable / 0 missed，即 **100% 已杀死**。这说明该模块现有测试质量已经很好，变异测试在此处的收益是「防止未来退化」而非「发现当前缺陷」。若实施时 `path_util.rs` 同样接近 100%，应在 Spec/Plan 中如实降低这三处的预期收益陈述，不得为了证明投入合理而夸大发现。

### D5：12 个存活变异体 —— 列为必修

不采用「只记录 72.7% 基线」的方案。理由：设一个「不许低于 72.7%」的门禁，等于承认 27% 的 bug 抓不到是正常的；而这 27% 是已知的、可修的、成本约 40 行测试。修复内容包括：

1. 为 `tool_level_allows` 缺测的四个 arm（`process_start` / `process_kill` / `write` / `edit`）各补一条「工具层为 true 时应 Allow」的用例——现有只有 `check_tool_level_bash_allow` 覆盖了 `bash`
2. 为 `apply_decision` 的 `process_start` / `process_kill` / `edit` arm 补对应用例（现有仅覆盖 `bash` 与 `write`）
3. 为 `load` 的 `ErrorKind::NotFound` 分支补用例（变异体 `:350` 存活）
4. 为 `check` 的 `is_yolo` 返回值变异补用例（`:127` 两个方向都存活，说明 `is_yolo()` 本身无直接断言——现有测试在 `runtime/src/bootstrap.rs:1420` 才断言，属跨 crate 的间接覆盖）

## 总体设计

### 组件划分

四个新组件，彼此独立，可分别实施与验证：

| 组件 | 载体 | 触发方式 | 依赖 |
|---|---|---|---|
| C1 patch coverage | `just ci-patch` recipe | 手动，合并收尾时 | `cargo-llvm-cov`、`diff-cover` |
| C2 变异测试 | `just mutants` recipe + 三处落点配置 | 手动 / nightly | `cargo-mutants` |
| C3 属性测试 | 各 crate 的 `tests/proptest_*.rs` | 随 `just test` | `proptest` |
| C4 golden-set eval | `tests/eval/` + `just eval` recipe | 手动 / nightly | 真实 API key |
| C5 nightly | `.github/workflows/nightly.yml` + `.gitlab-ci.yml` schedule | 定时 | C1–C4 的 recipe |

### 为什么不用「全仓变异测试」

实测 `permission.rs` 单文件 50 变异体耗时 123.7s。全仓 18.1 万行生产代码，按此比例推算需数十小时，且 `cargo-mutants` 每个变异体都要重新编译。这不是优化问题，是量级问题。限定三处是唯一可行解。

### 为什么 patch coverage 不设在总阈值

全仓行覆盖已 90.11%，设总阈值只会被存量拉平而失去信号。真正有价值的是「这次新增的代码有没有被测」——`diff-cover` 原生支持这个语义。

## 组件设计

### C1：patch coverage

**工具链**

- `cargo-llvm-cov`（实测安装 61s，`--locked`）
- `diff-cover`（Python 工具；本机 Python 3.13 为 externally-managed，只能用 venv 或 `pipx` 安装，实测 venv 方式可用）

**recipe 设计**

```just
# 只卡新增行的覆盖率，不设总阈值。默认 60%，可在调用时覆盖。
# 必须用独立 target：instrumented 编译会让主 target 缓存失效，
# 混用会导致每次正常 CI 都全量重编。
ci-patch base="main" min="60":
    #!/usr/bin/env bash
    set -euo pipefail
    export CARGO_TARGET_DIR=target/llvm-cov-target
    cargo llvm-cov --workspace --lcov --output-path target/llvm-cov-target/lcov.info
    diff-cover target/llvm-cov-target/lcov.info \
        --compare-branch={{base}} \
        --fail-under={{min}}
```

**关键约束（已实测）**

1. **测试失败会让 lcov 完全不产出**。实测 `--no-fail-fast` 也不行——只要有任一测试失败，`cargo llvm-cov` 就以 exit 101 中止，`--output-path` 指定的文件根本不存在。因此 `ci-patch` 的前置条件必须是全量测试全绿。
2. **`--compare-branch` 只对「当前分支 vs 基线分支」正确**。实测对历史 merge commit 用 `--compare-branch=<sha>^1` 会给出错位行号（同一段缺失行在三次不同比对里重复出现），不可用于审计历史。
3. **基线分支不存在时 `diff-cover` 返回 exit=1**（已实测），恰好符合「基线缺失必须判失败」的要求，无需额外包装。
4. **独立 target 占 7.2 GB**。`ci-patch` 结束不清扫；需要回收时手动 `rm -rf target/llvm-cov-target`。

**判定语义**

- 新增行覆盖 < 阈值 → 非 0 退出
- 无新增代码 → 跳过（`diff-cover` 输出 "No lines with coverage information in this diff."，exit 0）
- 基线分支不存在 → 非 0 退出（工具原生行为）

### C2：变异测试

**三处落点与预期规模**

| 落点 | 生产代码行 | 现有单测 | 实测耗时 | 实测基线 |
|---|---|---|---|---|
| `core/src/permission.rs` | 383 | 36 | 123.7s / 50 变异体 | 32 caught / 12 missed / 1 timeout（72.7%） |
| `llm/src/sse.rs` | 267 | 10 | **实测 3m / 11 变异体** | 4 caught / 7 unviable / **0 missed（100%）** |
| `tools/src/fs/path_util.rs` | 459 | 21 | 前置完成后实测 | 前置完成后实测 |

> **三条实测结论已改变设计判断**：
> 1. `sse.rs` 只有 11 个可测变异体、0 个存活——它的测试质量已经很好，变异测试在此处**没有发现新问题**。保留它的价值在于「防止未来退化」，而非「修复当前缺陷」。这一点必须在实施时如实记录，不要为了凑够三处而夸大收益。
> 2. `sse.rs` 单处耗时 3 分钟（其中基线编译 52s），比 `permission.rs` 的 123.7s 更长。三处合计约 **8–10 分钟**，确认不适合进每次 `ci`。
> 3. `path_util.rs` 存在 2 个必须先修的前置障碍（见上），**不能默认「配好 recipe 就能跑」**。

**实测障碍（2026-11-02，必须先在实施阶段解决，不是可选项）**

对 `path_util.rs` 首轮变异测试直接失败，退出码 4（`BaselineFailed`：未变异的代码本身就测试失败）。**连续发现两个独立障碍**，逐个记录：

**障碍 1：fixture 路径穿出 crate 树**

```
FAILED   Unmutated baseline in 23s build + 17s test
failures:
    fs::read_document::tests::reads_a_docx_keeping_headings_and_tables
    fs::read_document::tests::reads_a_pdf_and_reports_missing_text_layer
    ... 共 7 个
called `Result::unwrap()` on an `Err` value: Os { code: 2, kind: NotFound }
```

根因：`cargo-mutants` 只把 crate 树复制到 `target/mutants/<name>/`，而 `read_document.rs:475` 的 fixture 路径是 `CARGO_MANIFEST_DIR/../../../docs/research/fixtures/...`。`docs/` 在**仓库根**（`yi-agent-rs/` 之外），三个 `../` 恰好穿出 `yi-agent-rs/`，在复制树里必定落空。已实测确认复制树内 `docs/` 不存在。

**解法（已实测验证可行）**：给 fixture 路径加环境变量覆盖。

```rust
std::path::Path::new(&std::env::var("YI_FIXTURE_DIR").unwrap_or_else(|_| {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../docs/research/fixtures/2026-10-07-document-samples")
        .display().to_string()
})).join(name)
```

实测结果：
- 不带 `YI_FIXTURE_DIR`：`cargo test -p yi-agent-tools --lib read_document` → 17 passed，**默认行为完全不变**
- 带 `YI_FIXTURE_DIR=<绝对路径>`：同样 17 passed

该改动语义也合理——「fixture 位置可被外部指定」本就是测试基础设施该有的属性，不影响生产代码。

**障碍 2：`sandbox_readonly` 测试依赖 `.git` 目录**

解决障碍 1 后，立刻暴露第二个失败：

```
---- read_only_sandbox_can_run_git stdout ----
panicked at crates/yi-agent-tools/tests/sandbox_readonly.rs:48:5:
git must run under the read-only sandbox; exit=Some(128)
stderr=fatal: not a git repository (or any of the parent directories): .git
```

根因：该测试在 crate 目录里执行 `git log`，依赖仓库的 `.git`。`cargo-mutants` 复制树时默认不带 VCS，且其 `--copy-vcs` 复制的是 `yi-agent-rs/.git`（不存在），真实的 `.git` 在仓库根。

**解法（已实测验证）**：通过 `.cargo/mutants.toml` 排除该测试。

```toml
additional_cargo_test_args = ["--skip", "read_only_sandbox_can_run_git"]
```

实测结果（**全量，含集成测试**）：

```
cargo test -p yi-agent-tools -- --skip read_only_sandbox_can_run_git
test result: ok. 288 passed; 0 failed ... (lib)
test result: ok. 6 passed;  0 failed ... (集成)
test result: ok. 1 passed;  0 failed; 1 filtered out   ← 被排除的正是它
... 全部 ok
```

注意 `--skip` 必须走配置文件或 `-- <args>` 形式；直接写成 `-- --skip xxx` 会被 cargo 解析成测试名而报 `unexpected argument '--skip' found`（已实测踩坑）。

> 自查提示：首次验证时误用了 `--lib`，那只跑库单测、根本触及不到 `sandbox_readonly`（集成测试），得到的「288 passed」是假验证。已用无 `--lib` 的全量命令重跑确认。

**这两个障碍合起来意味着什么**

`path_util.rs` 无法「直接」做变异测试，必须先完成 2 处前置改动（1 处代码 + 1 个配置文件）。这属于**真实工作量**，Plan 里应作为独立任务前置，不能默认「配个 recipe 就能跑」。若实施时发现还有第三个隐藏障碍，应及时回退到备选落点。

**备选落点**：`crates/yi-agent-core/src/autonomy.rs`（62 行 / 4 单测，yolo 开关的单一事实来源）。安全价值低于路径边界，届时需重新评估第三处是否值得保留。

**recipe 设计**

```just
# 变异测试：只打三处安全关键模块。
# 三处实测：permission 123.7s + sse 3m + path_util（前置完成后实测）≈ 合计 8-10 分钟，
# 不适合进每次 ci。nightly 跑全量，手动调试用 --in-diff。
mutants:
    cargo mutants --file crates/yi-agent-core/src/permission.rs --timeout 60 -j 8
    YI_FIXTURE_DIR="{{justfile_dir}}/../docs/research/fixtures/2026-10-07-document-samples" \
        cargo mutants --config .cargo/mutants.toml \
        --file crates/yi-agent-tools/src/fs/path_util.rs --timeout 60 -j 8
    cargo mutants --file crates/yi-agent-llm/src/sse.rs --timeout 60 -j 8
```

`yi-agent-rs/.cargo/mutants.toml`：

```toml
# cargo-mutants 复制源码树到 target/mutants/<name>/ 时不带仓库根的 .git，
# 因此依赖 git 仓库的集成测试在复制树里必定失败（exit 128）。
# 基线失败会让整轮变异测试中止（exit 4），必须在测试层排除。
additional_cargo_test_args = ["--skip", "read_only_sandbox_can_run_git"]
```

> 这两个前置（`YI_FIXTURE_DIR` 支持 + `mutants.toml`）**必须先完成**，否则 `path_util.rs` 那一路永远以 exit 4 中止。

**门禁语义（源码 `cargo-mutants/src/exit_code.rs` 权威定义）**

- 0 = 全部变异体被杀死
- 1 = 参数错误（Clap 同值）
- 2 = 存在存活变异体（`FoundProblems`）
- 3 = 存在测试超时（`Timeout`，通常是变异导致死循环，或 timeout 设得太低）
- 4 = 未变异的代码本身就测试失败（`BaselineFailed`）
- 5 / 6 = `--in-diff` 相关内容不匹配或无法解析
- 70 = 内部错误

**重要**：退出码 2 和 3 是**不同**的问题，不能混判。本次实测 `permission.rs` 得到 `EXIT=3`，原因是 1 个 `TIMEOUT`（`==` → `!=` 变异造成死循环），而不是 12 个 missed —— 12 个 missed 与 1 个 timeout 同时存在时，工具只报 3。因此门禁脚本必须**解析 `outcomes.json` 或 stdout 统计**，不能只看退出码。

**「不得新增存活体」如何判定**

由于退出码无法区分「几个 missed」，判定必须基于结构化产物。`cargo-mutants` 会在 `target/mutants/` 下产出结果，可用 `--output` 指定 JSON：

- 每次运行后，从输出中提取 `missed` 与 `timeout` 两类条目，格式为 `{crate}:{line}:{col}: {description}`
- 与 `yi-agent-rs/mutants-baseline/{module}.txt` 逐条比对
- **出现基线中没有的新条目 → 失败**；基线中有但本次未出现 → 提示「可移除豁免」
- `caught` 与 `unviable` 不参与比对

`timeout` 单独记入 `{module}.timeout.txt`，因为它是「测试太慢/变异致死循环」，与「测试没抓到」是不同性质的问题。本次实测的 1 个 timeout（`permission.rs:133` 的 `==`→`!=`）应归此类，不计入存活。

这使得存量豁免显式化，且新增问题无法悄悄溜过。

### C3：属性测试

**依赖引入**

`proptest` 在本仓库**完全未引入**（`Cargo.toml` 与 `Cargo.lock` 均无）。需在 workspace 的 `[workspace.dependencies]` 添加，并在目标 crate 的 `[dev-dependencies]` 引用。

**三个被测函数与性质**

| 目标 | 入口 | 性质（property） |
|---|---|---|
| markdown 渲染 | `crates/yi-agent/src/tui/markdown.rs:27` `render_markdown(src: &str, width: u16)` | 1) 任意输入不 panic；2) 返回的 `Vec<Line>` 在有限步内产出（无死循环）；3) 任意 `width > 0` 时不产生宽度为 0 的行 |
| 折行 | `crates/yi-agent/src/tui/wrap.rs:42` `wrap_by_display_width(text: &str, width: usize, first_prefix: &str, cont_prefix: &str) -> Vec<String>` | 1) 任意输入不 panic；2) 拼接还原后语义不丢字（除空白归一）；3) 输出每行显示宽度 ≤ `width`（前缀计入） |
| slash 命令解析 | `crates/yi-agent/src/tui/slash.rs:223` `SlashCommand::from_name(name: &str)` | 1) 任意输入不 panic；2) 返回值恒为 `Option`，不存在「匹配到但内容越界」；3) 对 `all()` 里每个命令名，`from_name(name).name() == name`（往返一致） |

**为什么是这三处**：全部是解析外部/用户输入、且无 IO 依赖的纯函数。属性测试的价值在探索输入空间边界（畸形嵌套、超长 CJK、控制字符），而非补覆盖率——这三处测试密度本已不低（markdown 2234 行 / 58 测试，slash 1282 行 / 52 测试，wrap 223 行 / 11 测试）。

**规模约束**：每个性质设 `PROPTEST_CASES` 默认 256，CI 上不提高。发现问题后以最小的 failing case 转成常规单元测试固化，属性测试本身保留回归。

**已知风险**：`render_markdown` 依赖 `syntect`（语法高亮），可能对某些输入极慢。若实测单 case 超 1s，应把该性质排除 syntect 路径（用 `render_markdown_with` 指定 `TuiTheme` 的纯色主题）。

### C4：golden-set eval

这是本波唯一没有现成基建的组件，需要新建目录结构与 runner。

**目录结构**

```
yi-agent-rs/crates/yi-agent/tests/eval/
├── mod.rs              # runner：加载 case、执行、比对轨迹、汇总报告
├── cases/
│   ├── 001-read-and-edit.toml
│   ├── 002-fix-failing-test.toml
│   └── ...
└── baseline/
    └── 001-read-and-edit.json   # 首次通过时记录的轨迹快照
```

**case 格式（TOML）**

```toml
id = "001-read-and-edit"
prompt = "把 src/lib.rs 里 add 函数的返回值从 a-b 改成 a+b，然后跑 cargo test"

# 结构化初始状态：runner 复制到临时目录后执行
[fixture]
files = { "src/lib.rs" = "pub fn add(a: i32, b: i32) -> i32 { a - b }\n" }

[expect.result]
# 结果断言：文件存在 + 内容正则
files = { "src/lib.rs" = "a \\+ b" }

[expect.trajectory]
# 轨迹断言（本 Spec 的核心，区别于现有 e2e_complex）
must_call = ["read", "edit"]        # 必须调用的工具，顺序不限
must_not_call = ["bash"]            # 本例不该用 shell
max_tool_calls = 6                  # 多余步骤上限
max_input_tokens = 20000            # token 消耗上限
```

**轨迹评分实现**

`AgentEvent` 已提供所需全部信息，通过 `yi-agent run --json` 的 JSONL 事件流获取：

- 工具调用序列 → `AgentEvent::ToolCall { .. }`
- 工具成败 → `AgentEvent::ToolResult { .. }` / `ToolExit { .. }` / `ToolTimeout { .. }`
- token 消耗 → `AgentEvent::Usage { .. }`

复用 `tests/common/mod.rs` 现有的 `parse_events` / `event_variant` / `run_agent_with_timeout`（463 行，已含超时与 JSONL 解析），不重复造。

**规模与成本**

- 起步 30 条 case（清单建议 30–50，取下限以控制成本）
- 单例超时沿用现有 `COMPLEX_TIMEOUT = 300s`
- 按实测均速估算，30 条一轮约 15–45 分钟真实 API 调用

**执行时机**：不进 `just ci`，只进 `just eval`（手动）与 nightly。理由是成本（真钱）与耗时（分钟级）。

**基线更新流程**：轨迹或结果发生变化时，`just eval --bless` 重新生成 `baseline/*.json`，diff 必须人工审阅后提交。**禁止 CI 自动 bless**——那会让 eval 退化成「记录当前行为」，失去回归价值。

### C5：nightly

**现状**：两个 CI 配置均**无任何 `schedule` / `cron`**（实测 `grep` 零命中）。

**挂在哪**：GitHub Actions。理由：GitLab runner 是局域网内的 Mac mini（`10.79.10.70`），夜间跑真 LLM 测试会占用开发机；GitHub 云端 runner 隔离且不计本地资源。

**内容与耗时预算**

| 任务 | 预估耗时 | 失败影响 |
|---|---|---|
| `cargo audit` | 67s（实测） | 新增漏洞 → 告警 |
| `just mutants` | 8–10 分钟（三处，含 path_util 前置完成后） | 新增存活体 → 失败 |
| `just ci-patch` | 3–5 分钟（含全量重编） | 趋势记录，不阻断 |
| `just test-real-all` | 15–60 分钟（未实测） | 真实 LLM 回归，失败即告警 |
| `just eval` | 15–45 分钟（未实测，按 30 例 × 300s 上限估算） | 轨迹回归，失败即告警 |

**关键决策：`cargo audit` 进 `just ci`，不等 nightly**

实测 67 秒、不需要编译。新披露的 CVE 针对的是**没改动过的依赖**——等待开发者触碰相关代码才暴露是不可接受的。因此：

- `cargo audit` → 加入 `just ci` 依赖列表（秒级门禁，挡住新增）
- nightly 里再跑一次，用于捕获「依赖没变但新 CVE 被披露」的情况

**豁免清单**：D1 第三类的 8 个 unmaintained crate + `regex`（传递依赖）需录入 `yi-agent-rs/.cargo/audit.toml` 的 `[advisories] ignore`，每条必须写明理由与复查条件。

## 验证方式

每项都必须有可执行命令作为完成判据，不接受「已实现」这类主观描述。

### C1 patch coverage

```bash
# 1) 正常路径：造一个只改测试、不改生产的 diff，覆盖率应为 100%
git checkout -b probe/ci-patch-test
echo '// probe' >> yi-agent-rs/crates/yi-agent-core/src/lib.rs   # 生产代码，无测试
cd yi-agent-rs && just ci-patch        # 期望：非 0 退出，报告该行未覆盖

# 2) 反向验证：只改测试文件，期望 exit 0
# 3) 基线不存在：just ci-patch nonexistent-branch  期望 非 0 退出
```

**「门禁的价值在于它会红」**：必须包含一次故意破坏后变红的验证，只看到绿灯不算完成。

### C2 变异测试

```bash
cd yi-agent-rs && just mutants
# 判据：三处均 exit 0；存活体清单与 mutants-baseline/*.txt 完全一致
# 反向验证：手动在 permission.rs 里删掉一条 if 的 else 分支，期望 just mutants 报新存活体
```

**实测基线（2026-11-01，本机）**：`permission.rs` = 50 变异体 / 32 caught / 12 missed / 1 timeout，耗时 123.7s。修复 D5 后重测，期望 missed 降到 0。

### C3 属性测试

```bash
cd yi-agent-rs && cargo test -p yi-agent --test proptest_tui
# 判据：三条性质各跑 256 case 全绿，总耗时 < 60s
# 反向验证：把 wrap 的宽度上限逻辑改成恒返回 0，期望性质 3 失败
```

### C4 golden-set eval

```bash
cd yi-agent-rs && just eval
# 判据：30 条 case 全绿（结果断言 + 轨迹断言），报告输出每条的实际工具序列
# 反向验证：把某条 case 的 max_tool_calls 改成 1，期望该条失败
```

### C5 nightly

```bash
# 判据：workflow 文件存在且 cron 表达式合法
gh workflow list | grep nightly
# 反向验证：手动触发一次（workflow_dispatch），确认四个任务真的跑了
```

## 风险

| 风险 | 症状 | 应对 |
|---|---|---|
| `cargo update` 与其他改动冲突 | Cargo.lock 冲突、编译失败 | D1 已定：**等所有并行卡片合入后再单独执行**，不与任何改动并行 |
| `cargo-mutants` 全量编译慢 | 三处合计超 10 分钟 | `--in-diff` 可选；nightly 里跑全量，手动只跑 `--in-diff` |
| `TIMEOUT` 变异体误判为存活 | 死循环变异计入门禁失败 | 单独统计，不计入存活；`--timeout 60` 转为可判定结果 |
| `proptest` 发现 markdown 渲染性能问题 | 单 case 超 1s 导致测试超时 | 指定纯色 `TuiTheme` 绕开 syntect；必要时降低 case 数 |
| eval 成本失控 | 30 条 × 多次 = 真实账单 | 只进 nightly 与手动；单例 300s 超时上限；起步取下限 30 条 |
| eval baseline 被无脑 bless | 回归退化成「记录当前行为」 | 禁止 CI 自动 bless，必须人工审阅 diff |
| 覆盖率 target 占满磁盘 | 7.2 GB 常驻 | recipe 注释写明清理命令；worktree 收尾时提示 |
| **flaky 未彻底消除** | 见下节「已知遗留问题」 | 门禁前置条件加「全量测试全绿」，天然挡住 |

## 已知遗留问题（本波不解决，但必须记录）

第一波 Spec 修复了多数脆弱测试，但 **2026-11-02 在合入后的 main 上实测，三轮全量仍各有 1 个失败**：

```
RUN 1: 3953 passed, 1 failed  (a_completed_item_never_trails_its_own_buffered_delta, 34.4s)
RUN 2: 3953 passed, 1 failed  (同上, 34.8s)
RUN 3: 3953 passed, 1 failed  (a_process_that_owns_the_daemon_supervises_its_plugins, 12.3s)
```

相比修复前的「7 掉 / 2 掉 / 4 掉」已是量级改善，但**未归零**。已定位两处根因：

1. `crates/yi-agent-subagent/tests/attach_delegation.rs:572,587` 仍是固定 `Duration::from_secs(5)` 轮询，第一波只改了同文件的 652/757/837 行（改成 10s），572/587 漏改。
2. `rpc_tests.rs` 的 `try_read_value` 用 200ms 短超时轮询，高负载下帧被丢弃；`read_until_turn_completed` 最多读 40 帧，弃帧即 `panic!("turn/completed never arrived")`。

**影响本波**：C1 的前置条件「全量测试全绿」在 flaky 未归零前，会偶发挡住 `ci-patch`。这是行为正确的（覆盖率数据不可信时不该出报告），但会消耗耐心。

**建议**：flaky 归零作为独立任务优先处理，不塞进本波。判据是全量连跑 5 轮全绿。

## 附录 A：本次实测数据汇总

环境：macOS / Apple Silicon / 18 核 / 128 GB / 2026-11-01～02

| 项目 | 命令 | 耗时 | 结果 |
|---|---|---|---|
| 全量测试（修 flaky 前，热缓存） | `cargo test --all-features --workspace` | 49.4s | 1 failed（flaky） |
| 全量测试（nextest，修前） | `cargo nextest run --workspace` | 47–54s | 三轮分别 7/2/4 failed |
| 全量测试（nextest，修后） | 同上 `--profile ci` | 55–64s | 三轮各 1 failed |
| 覆盖率 | `cargo llvm-cov --workspace --lcov` | 165.7s | 行覆盖 90.11% |
| 变异测试 | `cargo mutants --file permission.rs` | 123.7s | 32 caught / 12 missed / 1 timeout（72.7%） |
| 依赖审计 | `cargo audit` | 66.9s | 30 条 / 19 crate |
| fmt 检查 | `cargo fmt --all -- --check` | 1.0s | 通过 |
| 凭据扫描 | `scripts/check-secrets.sh` | 0.2s | 通过 |
| doctest | `cargo test --workspace --doc` | — | **全部 0 个** |

## 附录 B：明确排除的方案

| 方案 | 为何排除 |
|---|---|
| 全仓变异测试 | 按 `permission.rs` 比例推算需数十小时，量级不可行 |
| 变异测试覆盖 store 幂等 | 真逻辑只靠集成测试，违反「只打快单元测试」约束（D4） |
| patch coverage 进 pre-push | 165s+ 且占 7.2 GB，单人开发必被 `--no-verify` 绕过（D2） |
| 设全仓覆盖率总阈值 | 已被 90.11% 存量填平，无信号价值 |
| golden eval 只评结果 | 现有 `e2e_complex` 已是此模式，测不出过程错误（D3） |
| golden eval 加稳定性统计 | 单轮成本 ×3–5，超出可接受范围（D3） |
| `serial_test` 掩盖 flaky | 第一波已论证：对跨二进制干扰无效，且掩盖问题 |
| 现在升级 quick-xml / zip | 跨大版本，混入测试改造会让 debug 变复杂（D1） |

## 附录 C：本波仍未覆盖、需要后续处理

| # | 事项 | 说明 |
|---|---|---|
| 1 | flaky 彻底归零 | 见「已知遗留问题」，两处根因已定位，判据为连跑 5 轮全绿 |
| 2 | `quick-xml` / `zip` 升级 | D1 第二类，3 个 high + 2 个 high，需单独回归 |
| 3 | insta 快照测试 | TUI 渲染与错误文案；CI 检测 `*.snap.new` 存在即失败 |
| 4 | 拆分巨型测试文件 | Rust：`rpc_tests.rs` 21434 行、`tui/app.rs` 12060 行、`server.rs` 10972 行；前端：`App.test.tsx` 6401 行、`ThreadSidebar.test.tsx` 2437 行 |
| 5 | JUnit 上传步骤 | 第一波已产出 `target/nextest/ci/junit.xml`，但无上传 |
| 6 | MSRV 门禁 | `Cargo.toml` 声明 `rust-version = "1.85"`，CI 无 job 用 1.85 编译 |
| 7 | 补真 doctest | 当前全仓 0 个，文档里的代码块从未被编译验证 |
| 8 | `CLAUDE.md` 门禁数量 | **已过期，需复核**：`CLAUDE.md:145` 现写「14 道」，`justfile` 的 `ci:` 依赖实测为 14 项，二者当前一致；但本波若把 `cargo audit` 加入 `ci`，需同步改成 15 |
| 9 | CI 分片 | 需先有实测数据判断是否必要 |
| 10 | `sandbox_readonly` 等集成测试的环境耦合 | `read_only_sandbox_can_run_git` 依赖仓库根 `.git`，在复制树/打包环境里不可用。本波只用 `mutants.toml` 跳过（治标）；根治应改为在测试内 `git init` 临时仓库 |
| 11 | fixture 路径依赖仓库布局 | C2 障碍 1 引入 `YI_FIXTURE_DIR` 是权宜之计；长期应把 `docs/research/fixtures/` 移进 crate 内，消除对仓库布局的隐式依赖 |

## 核查记录

- 2026-11-02：基于本机实测撰写初稿。所有性能数字、覆盖率、变异分数、漏洞数量均为实测值，无估算。
- 决策 D1–D5 均经项目所有者逐项确认。
- 三处变异落点、patch coverage 触发方式、eval 评分维度已与所有者对齐。
