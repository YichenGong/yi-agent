# 质量加固第二波 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把「90.11% 行覆盖但 72.7% 变异分数」这个实证缺口补上——修死 12 个存活变异体、建立 patch coverage 与变异测试门禁、给三个不可信输入 parser 加属性测试、建 Agent 轨迹级 golden-set eval、挂 nightly 定时任务。

**Architecture:** 五个互相独立的组件，按依赖顺序实施。先补测试（Task 1，纯收益、零风险），再建基础设施（Task 2–5，recipe + 配置），最后挂调度（Task 6）。每个组件单独可验证，任何一个中断都不影响已完成的。

**Tech Stack:** Rust 1.85 / cargo-nextest / cargo-llvm-cov / diff-cover / cargo-mutants / proptest / cargo-audit / GitHub Actions

## Global Constraints

以下约束适用于每一个 Task，不再重复：

- **Python 工具必须走 venv**：本机 Python 3.13 是 externally-managed，`pip install` 直接失败。`diff-cover` 装在 `/tmp/dcvenv/bin/diff-cover`；正式落地时建议装到 `~/.local/share/yi-agent-tools/venv`。
- **PATH 每次显式 export**：本机 shell 默认 PATH 只有 `/usr/bin:/bin:/usr/sbin:/sbin`，cargo/just/nextest 全在别处。所有命令前加：
  ```bash
  export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
  ```
- **只在 worktree 里干活**，不得在 main 上直接提交。分支名 `feat/quality-wave2`。
- **提交前必须 `cargo fmt --all`**，否则 `just ci` 的 `fmt-check` 直接红。
- **提交前必须 `just ci` 全绿**（在 `yi-agent-rs/` 下）。它是 14 道门禁的唯一入口。
- **commit message 不写 `Co-Authored-By`**；用 conventional commits，首行 ≤72 字符。
- **不要跑 `cargo update`**。D1 已定：那 134 个包的 lock 变更必须等所有并行卡片合入 main 后单独做，本 Plan 全部任务都不碰它。
- **不要动测试文件里 flaky 相关的部分**：`attach_delegation.rs:572/587`、`rpc_tests.rs` 的 `try_read_value` / `read_until_turn_completed` 有另一个进程在修，本 Plan 不参与。

---

## 文件结构

| 文件 | 责任 | Task |
|---|---|---|
| `crates/yi-agent-core/src/permission.rs` | 补 12 个存活变异体对应的单测（仅改 `mod tests`） | 1 |
| `yi-agent-rs/.cargo/mutants.toml` | 变异测试的测试排除配置 | 3 |
| `crates/yi-agent-tools/src/fs/read_document.rs` | fixture 路径支持 `YI_FIXTURE_DIR` 覆盖 | 3 |
| `yi-agent-rs/justfile` | 新增 `mutants` / `ci-patch` / `eval` / `audit` 四个 recipe | 2,3,4,5 |
| `yi-agent-rs/.cargo/audit.toml` | `cargo audit` 豁免清单 | 5 |
| `crates/yi-agent/tests/proptest_tui.rs` | 三个 parser 的属性测试 | 4 |
| `yi-agent-rs/Cargo.toml` + `crates/yi-agent/Cargo.toml` | 引入 `proptest` | 4 |
| `crates/yi-agent/tests/eval/mod.rs` | eval runner（加载 case、跑 agent、比对轨迹） | 5 |
| `crates/yi-agent/tests/eval/cases/*.toml` | 30 条 case 定义 | 5 |
| `crates/yi-agent/tests/eval_baseline.rs` | 驱动 `mod eval` 的 `#[ignore]` 测试入口 | 5 |
| `.github/workflows/nightly.yml` | nightly 调度 | 6 |
| `yi-agent-rs/mutants-baseline/*.txt` | 三处落点的存活体基线 | 3 |

**为什么 `eval` 拆成 `eval/mod.rs` + `eval_baseline.rs` 两层**：Cargo 的集成测试要求每个 `tests/*.rs` 是一个独立 crate root。`tests/eval/mod.rs` 是模块目录（不是测试文件），由 `tests/eval_baseline.rs` 里的 `mod eval;` 引入。这样 runner 逻辑可以单测，而真正的 agent 调用留在 `#[ignore]` 入口里。

---

## Task 1: 修死 permission.rs 的 12 个存活变异体

**Files:**
- Modify: `crates/yi-agent-core/src/permission.rs`（只在 `mod tests` 内追加，约 `:1002` 之后）

**Interfaces:**
- Consumes: 现有测试辅助函数 `checker_with(config: PermissionsConfig, yolo: bool) -> PermissionChecker`（`:463`）、`bash_input(cmd: &str) -> serde_json::Value`（`:479`）
- Produces: 无对外接口变化。这是纯测试补充，生产代码一行不改。

**背景**：2026-11-02 实测 `cargo mutants --file crates/yi-agent-core/src/permission.rs` 得到 50 变异体 / 32 caught / 12 missed / 5 unviable / 1 timeout。12 个存活体的精确位置见 Spec 的 D5 表格。**注意**：本 Task 只补测试，**绝对不要修改 `permission.rs` 里的任何生产代码**——那会让变异测试失去意义。

> **这 5 段测试代码已在写 Plan 时实际验证过**（2026-11-02）：把它们原样贴进 `mod tests` 后，
> 1. `cargo test -p yi-agent-core --lib permission` → **65 passed / 0 failed**（编译通过、全绿）
> 2. `cargo mutants --file crates/yi-agent-core/src/permission.rs --timeout 60 -j 8` → **50 mutants：0 missed / 44 caught / 5 unviable / 1 timeout**
>
> 即 12 个存活体**全部被杀死**，caught 从 32 升到 44。验证完已回滚，不在 Plan 分支留源码改动。
> 执行时若结果与此不符（例如只剩 11 个被杀死），说明代码在验证之后又有变动，按 Step 8 的做法重新定位。

- [ ] **Step 1: 建立基线，确认当前是 12 个 missed**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
cargo mutants --file crates/yi-agent-core/src/permission.rs --timeout 60 -j 8 2>&1 | tail -3
```

Expected: `50 mutants tested in 2m: 12 missed, 32 caught, 5 unviable, 1 timeouts`，退出码 3。

把这份输出存档，Step 8 要对比：

```bash
cargo mutants --file crates/yi-agent-core/src/permission.rs --timeout 60 -j 8 2>&1 | grep -E "^MISSED|^TIMEOUT" > /tmp/perm-baseline-before.txt
wc -l /tmp/perm-baseline-before.txt   # 期望 13（12 missed + 1 timeout）
```

- [ ] **Step 2: 补 `tool_level_allows` 四个缺失 arm 的测试（杀死变异体 #3–#6）**

在 `mod tests` 内、`check_tool_level_bash_allow`（`:484`）之后追加：

```rust
    /// 每个工具层开关单独打开时都必须真的放行。
    ///
    /// 缺了这条，`tool_level_allows` 里对应的 match arm 被删掉也不会红：
    /// 删除后落入 `_ => false`，而默认配置全是 false，两者结果相同。
    /// cargo-mutants 的 `:192/:193/:194/:195` 四个「delete match arm」
    /// 变异体正是靠这类用例才被杀死。
    #[test]
    fn check_tool_level_each_tool_allows_when_enabled() {
        let cases: [(&str, fn(&mut ToolLevelConfig)); 4] = [
            ("process_start", |c| c.process_start = true),
            ("process_kill", |c| c.process_kill = true),
            ("write", |c| c.write = true),
            ("edit", |c| c.edit = true),
        ];
        for (tool, enable) in cases {
            let mut tool_level = ToolLevelConfig::default();
            enable(&mut tool_level);
            let config = PermissionsConfig {
                tool_level,
                ..Default::default()
            };
            let checker = checker_with(config, false);
            let input = match tool {
                "write" | "edit" => serde_json::json!({"path": "/tmp/probe.txt"}),
                _ => serde_json::json!({"command": "probe"}),
            };
            assert!(
                matches!(checker.check(tool, &input), CheckResult::Allow),
                "{tool} 工具层开关为 true 时必须 Allow"
            );
        }
    }
```

- [ ] **Step 3: 补 `apply_decision` 三个缺失 arm 的测试（杀死变异体 #7–#9）**

`apply_decision` 的 `"write"` 已有 `apply_always_allow_tool_write_updates_config`（`:826`），但 `process_start` / `process_kill` / `edit` 没有。追加：

```rust
    /// `apply_decision` 的 `AlwaysAllowTool` 分支里，每种工具都要真的写进配置。
    /// 缺了这条，`:296/:297/:299` 三个「delete match arm」变异体存活——
    /// 删除后落入 `_ => {}`，配置不变，但没有测试会红。
    #[tokio::test]
    async fn apply_always_allow_tool_covers_every_arm() {
        let cases: [(&str, fn(&ToolLevelConfig) -> bool); 3] = [
            ("process_start", |c| c.process_start),
            ("process_kill", |c| c.process_kill),
            ("edit", |c| c.edit),
        ];
        for (tool, read) in cases {
            let tmpdir = tempfile::tempdir().unwrap();
            let workdir = tmpdir.path().to_path_buf();
            let blocklist: BlocklistFn = Arc::new(|_| None);
            let checker = PermissionChecker::new(
                PermissionsConfig::default(),
                crate::autonomy::YoloSwitch::new(false),
                workdir.clone(),
                blocklist,
            );

            checker
                .apply_decision(tool, &Decision::AlwaysAllowTool, &PermissionKind::Normal)
                .await
                .unwrap();

            let loaded = PermissionChecker::load(&workdir).await.unwrap();
            assert!(
                read(&loaded.tool_level),
                "{tool} 的 AlwaysAllowTool 必须落盘到 tool_level"
            );
        }
    }
```

- [ ] **Step 4: 补 write / edit 的 `AlwaysAllowPrefix` 去重测试（杀死变异体 #10–#11）**

现有 `apply_always_allow_prefix_bash_no_duplicate`（`:1002`）只覆盖 bash。`write` 和 `edit` 的去重守卫（`:312` / `:315`）没测。追加：

```rust
    /// `write` / `edit` 的 `AlwaysAllowPrefix` 重复添加必须去重。
    ///
    /// 缺了这条，`:312/:315` 的「match guard 恒为 true」变异体存活：
    /// 守卫失效后同一路径被反复 push，配置文件无限膨胀。
    /// 现有 `apply_always_allow_prefix_bash_no_duplicate` 只覆盖 bash。
    #[tokio::test]
    async fn apply_always_allow_prefix_write_and_edit_dedupe() {
        let cases: [(&str, fn(&mut PermissionsConfig, Vec<String>)); 2] = [
            ("write", |c, v| c.prefix_level.write.paths = v),
            ("edit", |c, v| c.prefix_level.edit.paths = v),
        ];
        for (tool, seed) in cases {
            let tmpdir = tempfile::tempdir().unwrap();
            let workdir = tmpdir.path().to_path_buf();
            let mut config = PermissionsConfig::default();
            seed(&mut config, vec!["src/**".to_string()]);

            let blocklist: BlocklistFn = Arc::new(|_| None);
            let checker = PermissionChecker::new(
                config,
                crate::autonomy::YoloSwitch::new(false),
                workdir.clone(),
                blocklist,
            );

            checker
                .apply_decision(
                    tool,
                    &Decision::AlwaysAllowPrefix("src/**".to_string()),
                    &PermissionKind::Normal,
                )
                .await
                .unwrap();

            let loaded = PermissionChecker::load(&workdir).await.unwrap();
            let paths = match tool {
                "write" => &loaded.prefix_level.write.paths,
                _ => &loaded.prefix_level.edit.paths,
            };
            assert_eq!(paths.len(), 1, "{tool} 不得重复添加已有前缀");
        }
    }
```

- [ ] **Step 5: 补 `load` 读取失败必须报错的测试（杀死变异体 #12）**

**这是本 Task 里最有价值的一条**。`:350` 的 `"with true"` 变异体把 `Err(e) => Err(...)` 兜底分支变成 `Ok(default)`，含义是：**配置文件损坏时静默回落到默认权限配置**。实测确认 `read_to_string` 对无效 UTF-8 返回 `InvalidData`、对目录返回 `IsADirectory`，都不是 `NotFound`，所以现有 `load_nonexistent_returns_default`（`:995`）杀不死它。

```rust
    /// 配置文件存在但读不出来时必须报错，不得静默降级为默认配置。
    ///
    /// 这是安全相关的：静默降级意味着损坏的 `permissions.toml` 被忽略，
    /// 权限全部回落到默认（更宽松）配置，而用户完全不知情。
    /// 覆盖变异体 `:350:23` 的 `"with true"`——它把兜底 `Err(e) => Err(...)`
    /// 改成了 `Ok(default)`，只有非 NotFound 的读取失败才能杀死它。
    ///
    /// 实测确认有效载荷：无效 UTF-8 字节 → `ErrorKind::InvalidData`（非 NotFound）。
    /// 不用「把目录当文件读」是因为那返回 `IsADirectory`，跨平台行为不一致。
    #[tokio::test]
    async fn load_unreadable_config_returns_error_not_default() {
        let tmpdir = tempfile::tempdir().unwrap();
        let dir = tmpdir.path().join(".yi-agent");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("permissions.toml"), [0xff, 0xfe, 0x00]).unwrap();

        let result = PermissionChecker::load(tmpdir.path()).await;
        assert!(
            result.is_err(),
            "损坏的配置文件必须报错，实际得到 {result:?}"
        );
    }
```

- [ ] **Step 6: 补 `is_yolo` 的直接断言（杀死变异体 #1–#2）**

`is_yolo` 在 `permission.rs` 内零测试引用，仅在 `runtime/src/bootstrap.rs` 跨 crate 间接断言，导致 `true`/`false` 两个方向都存活。

```rust
    /// `is_yolo` 是两个方向都要测的：改成恒 true 或恒 false 都必须被抓到。
    /// 在此之前 `permission.rs` 内零直接断言，仅靠 `runtime/src/bootstrap.rs`
    /// 的跨 crate 间接覆盖，故 `:127` 的 `with true` / `with false` 双双存活。
    #[test]
    fn is_yolo_reflects_the_switch() {
        let config = PermissionsConfig::default();
        let blocklist: BlocklistFn = Arc::new(|_| None);

        let on = PermissionChecker::new(
            config.clone(),
            crate::autonomy::YoloSwitch::new(true),
            std::path::PathBuf::from("/tmp"),
            blocklist.clone(),
        );
        assert!(on.is_yolo(), "YoloSwitch(true) 时 is_yolo 必须为 true");

        let off = PermissionChecker::new(
            config,
            crate::autonomy::YoloSwitch::new(false),
            std::path::PathBuf::from("/tmp"),
            blocklist,
        );
        assert!(!off.is_yolo(), "YoloSwitch(false) 时 is_yolo 必须为 false");
    }
```

- [ ] **Step 7: 跑测试确认全绿**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
cargo test -p yi-agent-core --lib permission 2>&1 | tail -5
```

Expected: `test result: ok.` 且总数比之前多 5（本次新增 5 个测试函数，覆盖 12 个变异体）。

- [ ] **Step 8: 重跑变异测试，确认 missed 归零**

```bash
cd yi-agent-rs
cargo mutants --file crates/yi-agent-core/src/permission.rs --timeout 60 -j 8 2>&1 | tee /tmp/perm-after.txt | tail -3
grep -c "^MISSED" /tmp/perm-after.txt
```

Expected: `0 mutants tested ...: 0 missed, 44 caught, 5 unviable, 1 timeouts`（caught 数会上升）。第二行 `grep -c` 输出 `0`。

若仍有 missed，逐个看 `grep "^MISSED" /tmp/perm-after.txt` 的行号，回到对应 Step 补断言。**不要在没归零时提交**。

- [ ] **Step 9: 生成基线文件**

```bash
cd yi-agent-rs
mkdir -p mutants-baseline
grep -E "^MISSED|^TIMEOUT" /tmp/perm-after.txt > mutants-baseline/permission.txt
cat mutants-baseline/permission.txt
```

Expected: 只有 1 行，是那个 TIMEOUT（`:133:22` 的 `replace == with !=`，40s build + 60s test）。timeout 不是「测试没抓到」，是变异导致死循环，按 Spec 单独归档。

- [ ] **Step 10: 跑 fmt + 提交**

```bash
cd yi-agent-rs
cargo fmt --all
git add crates/yi-agent-core/src/permission.rs mutants-baseline/permission.txt
git commit -m "test(permission): 补 12 个存活变异体对应用例

cargo mutants 实测 permission.rs 50 变异体 / 12 missed，全部集中在
「有配置项但没测读取路径」这类缺口：

- tool_level_allows 四个 arm（process_start/process_kill/write/edit）
  删除后落入 _ => false，默认配置下结果相同，故存活
- apply_decision 三个 arm 同理，删除后落入 _ => {}
- write/edit 的 AlwaysAllowPrefix 去重守卫失效会让配置无限膨胀
- load 的兜底 Err 分支被改成 Ok(default) 时，损坏的配置文件会被
  静默忽略、权限回落到更宽松的默认值（安全相关）
- is_yolo 在本文件内零直接断言，两个方向都存活

只改测试，生产代码未动。基线留下 1 个 timeout（:133 的 == 改 !=
导致死循环），单列不计入存活。"
```

---

## Task 2: patch coverage 门禁（`just ci-patch`）

**Files:**
- Modify: `yi-agent-rs/justfile`（在 `# === CI 入口 ===` 之前插入）

**Interfaces:**
- Consumes: 无（独立 recipe）
- Produces: `just ci-patch [base] [min]`，base 默认 `main`，min 默认 `60`。退出码 0 = 通过或无可比对行；非 0 = 覆盖率不足或基线分支不存在。

- [ ] **Step 1: 确认工具可用**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cargo llvm-cov --version
/tmp/dcvenv/bin/diff-cover --version
```

Expected: 分别输出 `cargo-llvm-cov 0.9.x` 和 `diff-cover 11.0.0`。

若 `diff-cover` 不在 `/tmp/dcvenv`（那是临时 venv，重启会丢），重建到稳定位置：

```bash
python3 -m venv ~/.local/share/yi-agent-tools/venv
~/.local/share/yi-agent-tools/venv/bin/pip install -q diff-cover
~/.local/share/yi-agent-tools/venv/bin/diff-cover --version
```

- [ ] **Step 2: 写 recipe**

在 `yi-agent-rs/justfile` 的 `# === CI 入口 ===` 注释**之前**插入：

```make
# === 质量度量（不进 ci，见 docs/superpowers/specs/2026-11-02-quality-hardening-wave2-design.md） ===

# 只卡「本次新增行」的覆盖率，不设全仓总阈值。
#
# 为什么不设总阈值：全仓行覆盖已 90.11%，设总阈值只会被存量拉平、失去信号。
# 为什么是手动 recipe 而非 pre-push hook：必须用独立 CARGO_TARGET_DIR 全量重编
# （占用 7.2 GB、165s 起），塞进每次 push 必被 --no-verify 绕过，等于没有。
#
# 前置条件：全量测试必须全绿。任何测试失败都会让 cargo llvm-cov 以 exit 101
# 中止且不产出 lcov 文件（实测确认 --no-fail-fast 也不行）。
ci-patch base="main" min="60":
    #!/usr/bin/env bash
    set -euo pipefail
    DC="$HOME/.local/share/yi-agent-tools/venv/bin/diff-cover"
    if [ ! -x "$DC" ]; then DC="$(command -v diff-cover)"; fi
    if [ ! -x "$DC" ]; then
        echo "找不到 diff-cover。安装：python3 -m venv ~/.local/share/yi-agent-tools/venv && ~/.local/share/yi-agent-tools/venv/bin/pip install diff-cover" >&2
        exit 1
    fi
    LCOV_DIR="target/llvm-cov-target"
    mkdir -p "$LCOV_DIR"
    CARGO_TARGET_DIR="$LCOV_DIR" cargo llvm-cov --workspace --lcov \
        --output-path "$LCOV_DIR/lcov.info"
    "$DC" "$LCOV_DIR/lcov.info" \
        --compare-branch="{{base}}" \
        --fail-under="{{min}}"
    echo "提示：$LCOV_DIR 占约 7.2 GB，回收用 rm -rf $LCOV_DIR"
```

- [ ] **Step 3: 验证 recipe 语法**

```bash
cd yi-agent-rs && just --list | grep ci-patch
```

Expected: 列出 `ci-patch base min`。

- [ ] **Step 4: 正向验证——未充分测试的新增代码必须变红**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
git checkout -b probe/ci-patch-red
printf '\n// probe: untested production line\npub fn __ci_patch_probe() -> u32 { 42 }\n' \
    >> crates/yi-agent-core/src/lib.rs
just ci-patch
echo "EXIT=$?"
```

Expected: `EXIT` 非 0，diff-cover 报告 `Coverage: xx%` 且 `Failure. Coverage is below 60%` 之类。**这一步会跑 165s 以上**（全量 instrumented 编译）。

- [ ] **Step 5: 反向验证——只改测试文件必须通过**

```bash
cd yi-agent-rs
git checkout crates/yi-agent-core/src/lib.rs
git checkout -b probe/ci-patch-green 2>/dev/null || true
printf '\n#[cfg(test)]\nmod probe_green {\n    #[test]\n    fn it_runs() { assert_eq!(super::__probe_absent_or_zero(), 0); }\n}\n' >/dev/null
# 不做真实改动，直接验证「无新增生产行」时的语义：
git stash list >/dev/null
just ci-patch 2>&1 | tail -5
echo "EXIT=$?"
```

Expected: `EXIT=0`，diff-cover 输出 `No lines with coverage information in this diff.` 或 `100%`。

> 若这一步因为「本分支相对 main 有 Task 1 的新增测试文件」而报告覆盖率不足，说明阈值语义正确——测试文件本身不计入覆盖率统计（diff-cover 只统计被 lcov 记录的源文件）。确认输出里没有 `crates/yi-agent-core/src/permission.rs` 的新增未覆盖行即可。

- [ ] **Step 6: 基线分支不存在时必须失败**

```bash
cd yi-agent-rs
just ci-patch nonexistent-branch-xyz 2>&1 | tail -5
echo "EXIT=$?"
```

Expected: `EXIT` 非 0。这是 `diff-cover` 的原生行为，不需要额外包装。

- [ ] **Step 7: 清理探针分支与临时产物**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git branch -D probe/ci-patch-red probe/ci-patch-green 2>/dev/null || true
git status --short
```

Expected: 工作区干净。

- [ ] **Step 8: 提交**

```bash
cd yi-agent-rs
git add justfile
git commit -m "ci: 加 just ci-patch 做新增行覆盖率门禁

只卡本次 diff 新增的行，默认阈值 60%，不设全仓总阈值——全仓已达
90.11%，总阈值只会被存量拉平而失去信号。

用独立 CARGO_TARGET_DIR（target/llvm-cov-target，约 7.2 GB）：
instrumented 编译会让主 target 缓存失效，混用会导致每次正常 CI 全量重编。

刻意不进 just ci：165s+ 且占 7.2 GB，单人本地开发塞进每次 push 必被
--no-verify 绕过。挂在合并收尾这一唯一有意义的时点。

三条实测约束已写进注释：测试失败时 lcov 不产出（--no-fail-fast 也不行）、
--compare-branch 只对分支比对正确、基线不存在时 diff-cover 原生返回非 0。"
```

---

## Task 3: 变异测试门禁（`just mutants` + 两处前置障碍）

**Files:**
- Create: `yi-agent-rs/.cargo/mutants.toml`
- Modify: `crates/yi-agent-tools/src/fs/read_document.rs:475` 附近（fixture 路径）
- Modify: `yi-agent-rs/justfile`
- Create: `yi-agent-rs/mutants-baseline/path_util.txt`、`sse.txt`

**Interfaces:**
- Consumes: Task 1 产出的 `mutants-baseline/permission.txt`
- Produces: `just mutants` 跑三处落点，全部 exit 0 为通过。

**关键背景**：`path_util.rs` **无法直接做变异测试**，实测连续踩到两个障碍（Spec 的 C2「实测障碍」一节有完整记录）。本 Task 必须先修这两处，否则那一路永远 exit 4。

- [ ] **Step 1: 先修障碍 1——fixture 路径支持环境变量覆盖**

```bash
cd yi-agent-rs
sed -n '470,485p' crates/yi-agent-tools/src/fs/read_document.rs
```

Expected: 看到 `env!("CARGO_MANIFEST_DIR")` 拼 `../../../docs/research/fixtures/...`。

改成（保持默认行为完全不变）：

```rust
/// fixture 目录：优先用 `YI_FIXTURE_DIR` 覆盖，否则回退到仓库内的默认位置。
///
/// 环境变量是为 `cargo-mutants` 准备的：它只把 crate 树复制到
/// `target/mutants/<name>/` 再编译，而默认路径里的 `../../../docs` 会穿出
/// crate 目录，在复制树里必定 NotFound。变异测试时注入绝对路径即可。
fn fixture_dir() -> std::path::PathBuf {
    match std::env::var_os("YI_FIXTURE_DIR") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../docs/research/fixtures/2026-10-07-document-samples"),
    }
}
```

然后把原来拼接 fixture 路径的地方改为 `fixture_dir().join(name)`。

- [ ] **Step 2: 验证默认行为未变**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
unset YI_FIXTURE_DIR
cargo test -p yi-agent-tools --lib read_document 2>&1 | tail -3
```

Expected: `test result: ok. 17 passed`（与改动前一致，实测基线是 17）。

- [ ] **Step 3: 验证环境变量覆盖生效**

```bash
cd yi-agent-rs
YI_FIXTURE_DIR="$PWD/../docs/research/fixtures/2026-10-07-document-samples" \
  cargo test -p yi-agent-tools --lib read_document 2>&1 | tail -3
```

Expected: 同样 `17 passed`。

- [ ] **Step 4: 修障碍 2——建 `mutants.toml` 排除依赖 git 的测试**

```bash
mkdir -p yi-agent-rs/.cargo
```

写入 `yi-agent-rs/.cargo/mutants.toml`：

```toml
# cargo-mutants 把源码树复制到 target/mutants/<name>/ 时不带仓库根的 .git，
# 因此任何依赖 git 仓库的测试在复制树里必定失败。
#
# 具体症状（实测）：sandbox_readonly.rs 的 read_only_sandbox_can_run_git
# 在 crate 目录里跑 `git log`，复制树里没有 .git →
#   fatal: not a git repository ... exit=Some(128)
# 基线失败会让整轮变异测试中止（exit 4），必须在测试层排除。
#
# 注意 --skip 必须走这个配置或 `-- <args>` 形式；写成 `cargo mutants -- --skip x`
# 会被 cargo 当成测试名解析而报 unexpected argument。
additional_cargo_test_args = ["--skip", "read_only_sandbox_can_run_git"]
```

- [ ] **Step 5: 验证排除后全量测试全绿（必须不带 `--lib`）**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
cargo test -p yi-agent-tools -- --skip read_only_sandbox_can_run_git 2>&1 | grep -E "^test result|FAILED" | tail -8
```

Expected: 全部 `ok`，其中一行是 `1 passed; 0 failed; 1 filtered out`（被排除的正是 `read_only_sandbox_can_run_git`）。

> **踩坑记录**：这一步**不能加 `--lib`**。`--lib` 只跑库单测，而 `sandbox_readonly` 是集成测试，加了 `--lib` 得到的「全绿」是假验证。

- [ ] **Step 6: 跑 path_util 的变异测试，拿基线**

```bash
cd yi-agent-rs
YI_FIXTURE_DIR="$PWD/../docs/research/fixtures/2026-10-07-document-samples" \
cargo mutants --config .cargo/mutants.toml \
  --file crates/yi-agent-tools/src/fs/path_util.rs --timeout 60 -j 8 \
  2>&1 | tee /tmp/mut-pathutil.txt | tail -3
echo "EXIT=${PIPESTATUS[0]}"
```

Expected: 输出 `N mutants tested in ...: X missed, Y caught, ...`，**不再出现 `BaselineFailed`**。

把这批 missed 存档：

```bash
grep -E "^MISSED|^TIMEOUT" /tmp/mut-pathutil.txt > yi-agent-rs/mutants-baseline/path_util.txt
wc -l yi-agent-rs/mutants-baseline/path_util.txt
```

> **如实记录要求**（Spec 的 D4 明令）：如果这里 missed 是 0，**必须**在提交信息里写明「该落点未发现缺陷，价值是防退化」，不得为了证明投入合理而夸大。若 missed 很多，先判断是否值得全部补测——超过 10 个则回 Spec 重新评估是否换落点。

- [ ] **Step 7: 跑 sse 的变异测试，补基线**

```bash
cd yi-agent-rs
cargo mutants --file crates/yi-agent-llm/src/sse.rs --timeout 60 -j 8 \
  2>&1 | tee /tmp/mut-sse.txt | tail -3
grep -E "^MISSED|^TIMEOUT" /tmp/mut-sse.txt > mutants-baseline/sse.txt
wc -l mutants-baseline/sse.txt
```

Expected: 与 2026-11-02 实测一致——11 变异体 / 4 caught / 7 unviable / 0 missed，耗时约 3 分钟。基线文件应为空（0 行）。

- [ ] **Step 8: 写 `mutants` recipe**

在 `justfile` 的 `ci-patch` recipe 之后追加：

```make
# 变异测试：只打三处安全关键模块（工具权限 / 文件路径边界 / 外部数据解析）。
#
# 三处实测合计约 8-10 分钟（permission 2m + sse 3m + path_util 视情况），
# 刻意不进 `ci`。nightly 跑全量，本地调试可用 --in-diff。
#
# 判据不是退出码——cargo-mutants 的退出码 2（有存活体）与 3（有超时）
# 是不同问题却会互相覆盖（本次实测 permission 有 12 missed + 1 timeout，
# 只报 3）。真正的判定必须比对 mutants-baseline/*.txt。
mutants:
    #!/usr/bin/env bash
    set -uo pipefail
    status=0
    run_one() {
        local name="$1"; shift
        echo "=== mutants: $name ==="
        "$@" 2>&1 | tee "/tmp/mut-$name.txt" | tail -3
        local rc=${PIPESTATUS[0]}
        local now
        now="$(mktemp)"
        grep -E "^MISSED|^TIMEOUT" "/tmp/mut-$name.txt" > "$now" || true
        local base="mutants-baseline/$name.txt"
        if [ ! -f "$base" ]; then
            echo "缺少基线 $base，先跑一次并把 /tmp/mut-$name.txt 的 MISSED 行存进去" >&2
            status=1; rm -f "$now"; return
        fi
        if ! diff -u "$base" "$now" > /tmp/mut-diff-$name.txt; then
            echo "!! $name 的存活体与基线不一致（新增或消失），见 /tmp/mut-diff-$name.txt" >&2
            status=1
        fi
        if [ "$rc" -ne 0 ] && [ "$rc" -ne 2 ] && [ "$rc" -ne 3 ]; then
            echo "!! $name 变异测试异常退出 rc=$rc（4=基线失败，70=内部错误）" >&2
            status=1
        fi
        rm -f "$now"
    }
    run_one permission cargo mutants --file crates/yi-agent-core/src/permission.rs --timeout 60 -j 8
    run_one sse cargo mutants --file crates/yi-agent-llm/src/sse.rs --timeout 60 -j 8
    run_one path_util env "YI_FIXTURE_DIR=$PWD/../docs/research/fixtures/2026-10-07-document-samples" \
        cargo mutants --config .cargo/mutants.toml \
        --file crates/yi-agent-tools/src/fs/path_util.rs --timeout 60 -j 8
    exit $status
```

- [ ] **Step 9: 全量跑一遍 `just mutants`，确认三处都通过基线比对**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
just mutants
echo "EXIT=$?"
```

Expected: `EXIT=0`，三处都输出「与基线一致」（无 diff）。**这一跑约 8-10 分钟**。

- [ ] **Step 10: 反向验证——故意制造一个新存活体**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
cp crates/yi-agent-core/src/permission.rs /tmp/permission.rs.bak
# 把 tool_level_allows 的 bash arm 改成恒 false——Step 2 的测试应该抓到它
python3 - <<'PY'
p='crates/yi-agent-core/src/permission.rs'
s=open(p).read()
s=s.replace('            "bash" => config.tool_level.bash,', '            "bash" => false,',1)
open(p,'w').write(s)
PY
cargo test -p yi-agent-core --lib permission 2>&1 | grep -E "^test result|FAILED" | tail -3
```

Expected: 测试**变红**（`check_tool_level_bash_allow` 与 `check_tool_level_each_tool_allows_when_enabled` 都应失败）。这证明 Task 1 的测试确实有效。验证完恢复：

```bash
cd yi-agent-rs && cp /tmp/permission.rs.bak crates/yi-agent-core/src/permission.rs && git status --short
```

Expected: 工作区干净（文件已还原）。

- [ ] **Step 11: 提交**

```bash
cd yi-agent-rs
cargo fmt --all
git add .cargo/mutants.toml crates/yi-agent-tools/src/fs/read_document.rs justfile mutants-baseline/
git commit -m "test: 变异测试门禁覆盖权限/路径/SSE 三处，并修两个前置障碍

path_util 无法直接做变异测试，实测连续踩到两个障碍，都先修掉：

1. fixture 路径 CARGO_MANIFEST_DIR/../../../docs 穿出 crate 树，
   cargo-mutants 的复制树里没有仓库根的 docs/ → 7 个测试 NotFound，
   整轮以 exit 4（BaselineFailed）中止。
   解法：YI_FIXTURE_DIR 环境变量覆盖，默认路径不变（实测 17 passed 一致）。
2. sandbox_readonly_can_run_git 在 crate 目录跑 git log，依赖仓库根 .git，
   复制树里不存在。解法：mutants.toml 的 additional_cargo_test_args --skip。
   （--skip 必须走配置或 -- <args>，直接拼在命令行会被当成测试名报错）

recipe 的判定不只看退出码：cargo-mutants 的 2（存活体）与 3（超时）会互相
覆盖，实测 permission 有 12 missed + 1 timeout 时只报 3。真正判定比对
mutants-baseline/*.txt，出现基线外的新条目才失败。

sse 实测 0 missed（100% 已杀死），该落点价值是防退化而非发现缺陷。"
```

---

## Task 4: 三个 parser 的属性测试

**Files:**
- Modify: `yi-agent-rs/Cargo.toml`（`[workspace.dependencies]`）
- Modify: `crates/yi-agent/Cargo.toml`（`[dev-dependencies]`）
- Modify: `crates/yi-agent/src/tui/wrap.rs`（在现有 `mod tests` 内追加）
- Modify: `crates/yi-agent/src/tui/markdown.rs`（同上）
- Modify: `crates/yi-agent/src/tui/slash.rs`（同上）

**Interfaces:**
- Consumes: 同文件内的 `wrap_by_display_width` / `render_markdown` / `SlashCommand::from_name`（私有可见性即可，因为测试在文件内）
- Produces: 三条属性的测试函数，随 `just test` 执行。

**关键约束（2026-11-02 实测确认）**：`yi-agent` 是**纯 binary crate**，`Cargo.toml` 只有 `[[bin]]` 没有 `[lib]`，`src/lib.rs` 不存在。`tui` 在 `src/main.rs:13` 声明为 `mod tui;`（**私有**）。因此**不能**新建 `tests/proptest_tui.rs` —— 集成测试是独立 crate，访问不到 bin 里的私有模块，会直接编译失败。

正确做法：属性测试写进**各源文件现有的 `mod tests`**。nextest 会采集 bin 的单测，实测确认：

```
$ cargo nextest list -p yi-agent | grep "tui::wrap::tests"
yi-agent::bin/yi-agent tui::wrap::tests::cjk_counts_as_two_columns
yi-agent::bin/yi-agent tui::wrap::tests::empty_input_yields_one_empty_line
...
```

**关键背景**：Spec 原稿的三条性质里**有两条经实测不成立**（见 Spec 的 C3「两条原性质为何被推翻」）。本 Task 写的是修正后的性质，**不要**照原稿写。

- [ ] **Step 1: 确认 proptest 的落点与采集方式**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
echo "--- 确认是纯 bin crate，无 lib.rs ---"
ls crates/yi-agent/src/lib.rs 2>/dev/null || echo "无 lib.rs（纯 binary）"
grep -n "^\[\[bin\]\]" -A2 crates/yi-agent/Cargo.toml

echo "--- 确认 tui 是私有 mod ---"
grep -n "mod tui" crates/yi-agent/src/main.rs

echo "--- 确认 nextest 采集 bin 单测 ---"
cargo nextest list -p yi-agent 2>/dev/null | grep -c "tui::wrap::tests"
```

Expected: 无 `lib.rs`；`[[bin]] name = "yi-agent"`；`main.rs` 里是 `mod tui;`（无 `pub`）；最后一行输出 `11`（wrap 现有 11 个单测被采集）。

**这四行输出共同决定了落点**：`tui` 不是 `pub mod`、也没有 `lib.rs` 转发，所以 `tests/proptest_tui.rs` 这种集成测试**编译不过**（集成测试是独立 crate，看不到 bin 的私有模块）。属性测试必须写进各源文件的 `mod tests`。

- [ ] **Step 2: 引入 proptest**

`yi-agent-rs/Cargo.toml` 的 `[workspace.dependencies]` 内追加（放在 `tempfile` 附近，与其它测试工具依赖并列）：

```toml
proptest = "1"
```

`crates/yi-agent/Cargo.toml` 的 `[dev-dependencies]` 内追加：

```toml
proptest = { workspace = true }
```

- [ ] **Step 3: 写 wrap 的两条性质（字符守恒 + 不 panic）**

在 `crates/yi-agent/src/tui/wrap.rs` 现有 `mod tests {`（`:114`）内部、最后一个测试之后追加。**不需要 `use` 语句**——同模块内 `use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};`（`:7`）和 `wrap_by_display_width` 都已在作用域内：

```rust
    proptest! {
        #![proptest_config(ProptestConfig {
            cases: std::env::var("PROPTEST_CASES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(256),
            ..ProptestConfig::default()
        })]

        /// 字符守恒：剥掉 prefix 后逐行拼接，必须与输入（去掉显式换行）逐字符相同。
        ///
        /// 这是本模块文档承诺的核心不变量：
        /// "Wrapping here never alters a character."
        /// 它也是「权限确认框里显示的命令必须与真正执行的完全一致」的底层保证。
        #[test]
        fn prop_preserves_every_character(
            text in "\\PC{0,120}",
            width in 1usize..60,
            first_prefix in "[ ]{0,4}",
            cont_prefix in "[ ]{0,4}",
        ) {
            let out = wrap_by_display_width(&text, width, &first_prefix, &cont_prefix);
            let mut rebuilt = String::new();
            for (i, line) in out.iter().enumerate() {
                let p = if i == 0 { &first_prefix } else { &cont_prefix };
                let body = line.strip_prefix(p.as_str())
                    .unwrap_or_else(|| panic!("line {i} 缺少预期 prefix {p:?}: {line:?}"));
                rebuilt.push_str(body);
            }
            let expected: String = text.chars().filter(|c| *c != '\n').collect();
            prop_assert_eq!(rebuilt, expected, "字符被改动或丢失");
        }

        /// 不 panic：任意输入（含控制字符、超长 CJK、空串）都不得崩。
        #[test]
        fn prop_never_panics(
            text in "\\PC{0,200}",
            width in 0usize..120,
            first_prefix in "\\PC{0,8}",
            cont_prefix in "\\PC{0,8}",
        ) {
            let _ = wrap_by_display_width(&text, width, &first_prefix, &cont_prefix);
        }

        /// 宽度约束**仅在 prefix 宽度 < width 时**成立。
        ///
        /// 实测反例：`wrap_by_display_width("hello world", 5, ">>>>> ", ">>>>>> ")`
        /// 产出 11 行、11 行全部超宽。根因是本文件的
        /// `width.saturating_sub(prefix_width).max(1)`——prefix 比 width 宽时
        /// 可用宽度被兜到 1，属于有意的降级行为（宁可超宽也要每行有内容），
        /// 不是 bug。所以这条性质必须带前置条件，否则会对正确实现报假失败。
        #[test]
        fn prop_respects_width_when_prefix_fits(
            text in "\\PC{0,120}",
            width in 4usize..80,
            first_prefix in "[ ]{0,3}",
            cont_prefix in "[ ]{0,3}",
        ) {
            prop_assume!(UnicodeWidthStr::width(first_prefix.as_str()) < width);
            prop_assume!(UnicodeWidthStr::width(cont_prefix.as_str()) < width);
            let out = wrap_by_display_width(&text, width, &first_prefix, &cont_prefix);
            for line in &out {
                prop_assert!(
                    UnicodeWidthStr::width(line.as_str()) <= width,
                    "行宽 {} 超过上限 {}：{:?}",
                    UnicodeWidthStr::width(line.as_str()), width, line
                );
            }
        }
    }
```

同时在 `mod tests` 顶部的 `use` 区补一行（`mod tests` 内原本 `use super::*;`，需要额外引入 proptest 宏）：

```rust
    use proptest::prelude::*;
```

- [ ] **Step 4: 跑 wrap 的属性测试**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent tui::wrap 2>&1 | tail -25
```

Expected: 编译通过，`prop_preserves_every_character` / `prop_never_panics` / `prop_respects_width_when_prefix_fits` 与原有 11 个测试全部 ok。

**若 `prop_preserves_every_character` 失败**：它抓到了真实缺陷。**不要修改属性去迁就实现**——proptest 会打印 `minimal failing input`，先判断是实现 bug 还是性质写错了。已知风险：若实现里有 trim 行为，`\r` 或连续空格会触发这里。

- [ ] **Step 5: 写 markdown 的性质（不 panic + 输出行数有界）**

在 `crates/yi-agent/src/tui/markdown.rs` 现有 `mod tests {`（`:1307`）内部追加。同样只需补 `use proptest::prelude::*;`：

```rust
    proptest! {
        #![proptest_config(ProptestConfig {
            cases: std::env::var("PROPTEST_CASES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(64),
            ..ProptestConfig::default()
        })]

        /// 任意 markdown 输入都不 panic，且 width == 0 也不崩。
        ///
        /// 不用「不产生零宽行」这类性质：空输入渲染出的就是 1 个空行（宽度 0），
        /// 那是合理行为，写成必须非零会对正确实现报假失败。
        ///
        /// case 数取 64（而非 256）：`just test` 用 `--all-features` 会打开
        /// `syntax-highlight`（syntect），markdown 渲染在此路径下明显变慢。
        #[test]
        fn prop_never_panics(src in "\\PC{0,400}", width in 0u16..120) {
            let lines = render_markdown(&src, width);
            prop_assert!(
                lines.len() <= src.chars().count() + 64,
                "输出行数 {} 与输入长度 {} 不成比例，疑似死循环或爆炸",
                lines.len(), src.chars().count()
            );
        }
    }
```

- [ ] **Step 6: 跑 markdown 的性质测试并对比两种 feature 组合的耗时**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
echo "--- 默认 features ---"
time cargo test -p yi-agent --bin yi-agent tui::markdown 2>&1 | tail -3
echo "--- all-features（just test 用的就是这个）---"
time cargo test -p yi-agent --all-features --bin yi-agent tui::markdown 2>&1 | tail -3
```

Expected: 两次都 ok。记录两次耗时差——这是判断 syntect 是否真的拖慢的依据（Spec 的 C3 已指出：`default = []` 时根本不编译 syntect，但 `just test` 用 `--all-features` 会打开它）。

若 `--all-features` 下耗时超过 60s，把上面属性里的 `unwrap_or(64)` 降到 `32`，并在测试注释里写明原因。

- [ ] **Step 7: 写 slash 的两条收敛性质**

在 `crates/yi-agent/src/tui/slash.rs` 现有 `mod tests {`（`:648`）内部追加。注意该文件的 `mod tests` 已有 `use super::*;`，`SlashCommand` 在作用域内：

```rust
    proptest! {
        #![proptest_config(ProptestConfig {
            cases: std::env::var("PROPTEST_CASES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(256),
            ..ProptestConfig::default()
        })]

        /// 任意输入都不 panic，且返回值的规范名能再次解析回它自己。
        ///
        /// 这是 `from_name` 真正有意义的不变量。原稿写的
        /// 「`from_name(name).name() == name` 对所有 `all()` 元素成立」是恒真废话
        /// ——`all()` 里存的就是规范名，`find(|c| c.name() == name)` 直接命中，
        /// 永远为真、测不出任何东西。
        #[test]
        fn prop_from_name_converges_on_canonical_name(name in "\\PC{0,40}") {
            if let Some(cmd) = SlashCommand::from_name(&name) {
                let canonical = cmd.name();
                prop_assert_eq!(
                    SlashCommand::from_name(canonical).map(|c| c.name()),
                    Some(canonical),
                    "规范名必须能往返解析"
                );
            }
        }

        /// 别名也要收敛：`from_name("kanban")` 得到 `Kanban`，
        /// 而 `Kanban.name()` 是 "superpowers-kanban"（`:89` 的 `name()` 实现）。
        /// 这条保证「用户输入别名」与「用规范名再解析」指向同一命令。
        #[test]
        fn prop_aliases_converge(name in "\\PC{0,60}") {
            let Some(first) = SlashCommand::from_name(&name) else { return Ok(()) };
            let second = SlashCommand::from_name(first.name());
            prop_assert_eq!(second, Some(first));
        }
    }
```

- [ ] **Step 8: 跑全部属性测试**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent 2>&1 | tail -8
```

Expected: 全部 ok，耗时增量 < 60s。

- [ ] **Step 9: 确认随 `just test` 一起跑**

```bash
cd yi-agent-rs
cargo nextest list -p yi-agent --all-features 2>/dev/null | grep -E "prop_preserves_every_character|prop_never_panics|prop_from_name_converges|prop_respects_width_when_prefix_fits|prop_aliases_converge"
```

Expected: 列出 5 条（wrap 3 + markdown 1 + slash 1），前缀都是 `yi-agent::bin/yi-agent tui::`。这证明 `just test`（= `cargo nextest run --all-features --workspace`）会带上它们。

- [ ] **Step 10: 跑 fmt + 提交**

```bash
cd yi-agent-rs
cargo fmt --all
git add Cargo.toml crates/yi-agent/Cargo.toml Cargo.lock \
    crates/yi-agent/src/tui/wrap.rs \
    crates/yi-agent/src/tui/markdown.rs \
    crates/yi-agent/src/tui/slash.rs
git commit -m "test(tui): 给三个不可信输入 parser 加属性测试

proptest 打 tui 下的 wrap / markdown / slash：全是解析用户输入的纯函数，
测试密度本已不低（58/52/11 个单测），属性测试的价值在探索边界输入而非
补覆盖率。

落点写在各文件的 mod tests 内，不是 tests/proptest_tui.rs：yi-agent 是纯
binary crate（无 lib.rs），tui 在 main.rs 里是私有 mod，集成测试访问不到。
nextest 会采集 bin 单测（yi-agent::bin/yi-agent tui::*），故一样进 just test。

写测试过程中实测推翻了 Spec 原稿的两条性质：

1. wrap 的「每行 ≤ width」不普遍成立。反例：
   wrap_by_display_width(\"hello world\", 5, \">>>>> \", \">>>>>> \")
   → 11 行全部超宽。根因是 width.saturating_sub(prefix).max(1)：
   prefix 比 width 宽时可用宽度被兜到 1，是有意降级。性质加了前置条件。
2. slash 的「from_name(n).name() == n 对所有 all() 元素成立」是恒真废话
   （all() 里就是规范名，find 直接命中）。改为两条真不变量：规范名往返、
   别名收敛（from_name(\"kanban\") → Kanban，其 name 是 superpowers-kanban）。

markdown 的 syntect 慢路径默认关闭（default = []），但 just test 用
--all-features 会打开它，故 case 数取 64 并实测对比了两种 feature 的耗时。"
```
## Task 5: Agent golden-set eval（评结果 + 评轨迹）

**Files:**
- Create: `crates/yi-agent/tests/eval/mod.rs`
- Create: `crates/yi-agent/tests/eval_baseline.rs`
- Create: `crates/yi-agent/tests/eval/cases/001-*.toml` 起 30 条
- Create: `crates/yi-agent/tests/eval/baseline/`
- Modify: `yi-agent-rs/justfile`

**Interfaces:**
- Consumes: `tests/common/mod.rs` 的 `parse_events(&str) -> Vec<serde_json::Value>`、`event_variant(&str) -> Option<String>`、`run_agent_with_timeout(&Path, &str) -> Result<Output, String>`、`skip_if_no_key() -> bool`
- Produces: `just eval [filter] [bless]`，退出码 0 = 全部 case 通过。

**为什么拆两个文件**：Cargo 要求 `tests/*.rs` 各为独立 crate root。`tests/eval/mod.rs` 是模块（被 `mod eval;` 引入），这样 runner 逻辑可单测，agent 调用留在 `#[ignore]` 入口。

- [ ] **Step 1: 建目录与 case 数据结构**

```bash
mkdir -p crates/yi-agent/tests/eval/cases crates/yi-agent/tests/eval/baseline
```

`crates/yi-agent/tests/eval/mod.rs`：

```rust
//! Golden-set eval runner：评「结果」与「轨迹」。
//!
//! 相对于 `e2e_complex.rs`（只断言结果文件），这里额外断言**过程**：
//! 该调的工具调了没有、不该调的多调了没有、步数与 token 有没有失控。
//! 这才能抓到「工具选错但碰巧把文件写出来」这类失败。
//!
//! 轨迹信息全部来自 `yi-agent run --json` 的 JSONL 事件流，无需改 core：
//! `AgentEvent::ToolCall { id, name, input }` / `ToolResult` / `ToolExit` /
//! `Usage { model, usage: TokenUsage { input_tokens, .. } }`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 一条 eval case 的完整定义，对应一个 TOML 文件。
#[derive(Debug, serde::Deserialize)]
pub struct Case {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub fixture: Fixture,
    pub expect: Expect,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct Fixture {
    /// 相对路径 → 文件内容。runner 写到临时目录后再跑 agent。
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct Expect {
    pub result: ResultExpect,
    pub trajectory: TrajectoryExpect,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct ResultExpect {
    /// 相对路径 → 期望内容的正则。文件必须存在且匹配。
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct TrajectoryExpect {
    /// 必须调用过的工具名。
    #[serde(default)]
    pub must_call: Vec<String>,
    /// 绝不允许调用的工具名。
    #[serde(default)]
    pub must_not_call: Vec<String>,
    /// 工具调用总数上限。
    pub max_tool_calls: Option<usize>,
    /// 累计 input token 上限。
    pub max_input_tokens: Option<u64>,
}
```

- [ ] **Step 2: 写轨迹提取与比对逻辑**

追加到 `mod.rs`：

```rust
/// 从 JSONL 事件流里抽出评测需要的轨迹信息。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Trace {
    /// 按发生顺序的工具名。
    pub tool_calls: Vec<String>,
    /// 累计 input token（各次 `Usage` 事件求和）。
    pub input_tokens: u64,
    /// 是否正常结束（`Done` 且 reason 为 `EndTurn`）。
    pub ended_normally: bool,
}

/// 从已解析的事件里提取轨迹。
///
/// 事件有两种编码：字符串变体（`"Done"`）和单键对象（`{"ToolCall": {...}}`）。
/// 这里两种都处理，与 `common::event_variant` 的判定口径一致。
pub fn extract_trace(events: &[serde_json::Value]) -> Trace {
    let mut trace = Trace::default();
    for ev in events {
        let Some(obj) = ev.as_object() else {
            if ev.as_str() == Some("Done") {
                trace.ended_normally = true;
            }
            continue;
        };
        if let Some(tc) = obj.get("ToolCall") {
            if let Some(name) = tc.get("name").and_then(|v| v.as_str()) {
                trace.tool_calls.push(name.to_string());
            }
        }
        if let Some(usage) = obj.get("Usage").and_then(|u| u.get("usage")) {
            if let Some(n) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                trace.input_tokens += n;
            }
        }
        if let Some(done) = obj.get("Done") {
            if done.get("reason").and_then(|v| v.as_str()) == Some("EndTurn") {
                trace.ended_normally = true;
            }
        }
    }
    trace
}

/// 按 `Expect` 校验轨迹，返回所有违反项（空 = 通过）。
pub fn check_trajectory(expect: &TrajectoryExpect, trace: &Trace) -> Vec<String> {
    let mut failures = Vec::new();
    if !trace.ended_normally {
        failures.push("未正常结束（缺 Done/EndTurn）".to_string());
    }
    for required in &expect.must_call {
        if !trace.tool_calls.iter().any(|t| t == required) {
            failures.push(format!(
                "must_call 未满足：没有调用 {required}（实际：{:?}）",
                trace.tool_calls
            ));
        }
    }
    for forbidden in &expect.must_not_call {
        if trace.tool_calls.iter().any(|t| t == forbidden) {
            failures.push(format!(
                "must_not_call 被违反：调用了 {forbidden}（实际：{:?}）",
                trace.tool_calls
            ));
        }
    }
    if let Some(max) = expect.max_tool_calls {
        if trace.tool_calls.len() > max {
            failures.push(format!(
                "工具调用数 {} 超过上限 {max}（实际：{:?}）",
                trace.tool_calls.len(),
                trace.tool_calls
            ));
        }
    }
    if let Some(max) = expect.max_input_tokens {
        if trace.input_tokens > max {
            failures.push(format!(
                "input token {} 超过上限 {max}",
                trace.input_tokens
            ));
        }
    }
    failures
}

/// 加载 `dir` 下全部 `*.toml`，按文件名排序。
pub fn load_cases(dir: &Path) -> Vec<(PathBuf, Case)> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("读 cases 目录 {} 失败：{e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("toml"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let raw = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("读 {} 失败：{e}", p.display()));
            let case: Case = toml::from_str(&raw)
                .unwrap_or_else(|e| panic!("解析 {} 失败：{e}", p.display()));
            (p, case)
        })
        .collect()
}
```

`crates/yi-agent/Cargo.toml` 的 `[dev-dependencies]` 需加（若尚无）：

```toml
toml = "0.8"
regex = "1"
```

- [ ] **Step 3: 写 case 执行与结果校验**

追加：

```rust
/// 在临时目录里铺好 fixture，跑 agent，返回 Trace 与 stdout。
pub fn run_case(
    bin: &Path,
    case: &Case,
) -> Result<(Trace, String), String> {
    let tmp = tempfile::TempDir::new().map_err(|e| e.to_string())?;
    for (rel, content) in &case.fixture.files {
        let path = tmp.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, content).map_err(|e| e.to_string())?;
    }

    let mut cmd = std::process::Command::new(bin);
    cmd.arg("--workdir")
        .arg(tmp.path())
        .arg("run")
        .arg("--json")
        .arg(&case.prompt);
    let out = crate::common::run_command_with_timeout(
        &mut cmd,
        std::time::Duration::from_secs(300),
    )?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let events = crate::common::parse_events(&stdout);
    let trace = extract_trace(&events);

    let mut failures = check_trajectory(&case.expect.trajectory, &trace);
    for (rel, pattern) in &case.expect.result.files {
        let path = tmp.path().join(rel);
        match std::fs::read_to_string(&path) {
            Ok(actual) => {
                let re = regex::Regex::new(pattern)
                    .map_err(|e| format!("{rel} 的正则非法：{e}"))?;
                if !re.is_match(&actual) {
                    failures.push(format!(
                        "{rel} 内容不匹配 /{pattern}/，实际：{actual}"
                    ));
                }
            }
            Err(e) => failures.push(format!("{rel} 读取失败：{e}")),
        }
    }
    if !failures.is_empty() {
        return Err(format!("case {} 失败：\n  {}", case.id, failures.join("\n  ")));
    }
    Ok((trace, stdout))
}
```

> **模块路径**：`run_command_with_timeout` 的签名是 `(&mut Command, Duration) -> Result<Output, String>`（`common/mod.rs:246`）。
>
> `crate::common::` 能成立的前提是 `eval_baseline.rs` 里同时写了 `mod common;` 和 `mod eval;` —— 这样 `eval` 是 crate root 的子模块，`crate::common` 指的就是它兄弟模块。**若编译报 `unresolved module crate::common`**，说明 `mod` 声明顺序或命名不一致，最简单的修法是在 `eval/mod.rs` 顶部显式写 `use crate::common;`，然后把正文里的 `crate::common::foo` 改成 `common::foo`。
>
> 另需在 `crates/yi-agent/Cargo.toml` 的 `[dev-dependencies]` 补两个依赖（lock 里已有传递版本，可直接声明）：
>
> ```toml
> toml = "0.8"      # Cargo.lock 已有 0.8.23（yi-agent-core 在用）
> regex = "1"       # Cargo.lock 已有 1.13.1（yi-agent-tools 在用）
> ```

- [ ] **Step 4: 写 `#[ignore]` 驱动入口**

`crates/yi-agent/tests/eval_baseline.rs`：

```rust
//! Golden-set eval 的驱动入口。
//!
//! 全部 `#[ignore]`：真钱（真实 API 调用）+ 分钟级耗时，不进 `just ci`。
//! 跑法：`cargo test -p yi-agent --test eval_baseline -- --ignored --nocapture`
//! 或 `just eval`。

mod common;
mod eval;

use std::path::PathBuf;

fn cases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/eval/cases")
}

#[test]
#[ignore]
fn golden_set_eval() {
    if !common::skip_if_no_key() {
        return;
    }
    let filter = std::env::var("EVAL_FILTER").unwrap_or_default();
    let cases = eval::load_cases(&cases_dir());
    assert!(!cases.is_empty(), "cases 目录里没有 case");

    let bin = common::yi_agent_bin();
    let mut failures = Vec::new();
    let mut ran = 0usize;

    for (path, case) in &cases {
        if !filter.is_empty() && !case.id.contains(&filter) {
            continue;
        }
        ran += 1;
        match eval::run_case(&bin, case) {
            Ok((trace, _)) => {
                println!(
                    "PASS {}  tools={:?} tokens={}",
                    case.id, trace.tool_calls, trace.input_tokens
                );
            }
            Err(msg) => {
                println!("FAIL {}  (from {})", case.id, path.display());
                failures.push(msg);
            }
        }
    }

    println!("\n跑了 {ran} 条 case，失败 {} 条", failures.len());
    assert!(failures.is_empty(), "失败详情：\n{}", failures.join("\n\n"));
}
```

- [ ] **Step 5: 写前 3 条 case 验证 runner 可用**

`cases/001-read-and-edit.toml`：

```toml
id = "001-read-and-edit"
prompt = "Read src/lib.rs, then change the body of the `add` function so it adds instead of subtracts. Use the edit tool."

[fixture]
files = { "src/lib.rs" = "pub fn add(a: i32, b: i32) -> i32 { a - b }\n" }

[expect.result]
files = { "src/lib.rs" = "a \\+ b" }

[expect.trajectory]
must_call = ["read", "edit"]
must_not_call = ["bash"]
max_tool_calls = 6
max_input_tokens = 20000
```

`cases/002-create-file.toml`：

```toml
id = "002-create-file"
prompt = "Create a file named notes.md containing exactly the single line: hello. Use the write tool."

[expect.result]
files = { "notes.md" = "^hello\\s*$" }

[expect.trajectory]
must_call = ["write"]
max_tool_calls = 4
max_input_tokens = 12000
```

`cases/003-no-shell-when-unneeded.toml`：

```toml
id = "003-no-shell-when-unneeded"
prompt = "Read the file config.txt and tell me what the `port` value is. Do not modify anything."

[fixture]
files = { "config.txt" = "host = localhost\nport = 8080\n" }

[expect.trajectory]
must_call = ["read"]
must_not_call = ["write", "edit", "bash"]
max_tool_calls = 4
max_input_tokens = 12000
```

- [ ] **Step 6: 跑 3 条 case 验证 runner**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
# 需要 API key，从 .env 加载（justfile 里其他 real 测试的做法）
set -a; source ../.env 2>/dev/null; set +a
cargo test -p yi-agent --test eval_baseline -- --ignored --nocapture 2>&1 | tail -25
```

Expected: 三条都 `PASS`，并打印各自的 `tools=[...]` 与 `tokens=...`。

> 若报「没有 API key」会直接 `return` 而测试通过——那是 `skip_if_no_key` 的设计。看到 `跑了 0 条 case` 说明 key 没加载到。

- [ ] **Step 7: 补到 30 条 case**

按 Step 5 的三种模板扩展到 30 条。覆盖面建议（每条一个 TOML，`id` 递增）：

| 编号段 | 主题 | 关键断言 |
|---|---|---|
| 001–005 | 单文件读写改 | `must_call` 精确到 read/write/edit；`must_not_call = ["bash"]` |
| 006–010 | 多文件重构 | `max_tool_calls` 收紧（抓到「反复读同一文件」）；结果正则 |
| 011–015 | 测试失败修复 | `must_call` 含 bash（跑测试）；`max_tool_calls` 放宽到 12 |
| 016–020 | 不该动文件的问答 | `must_not_call = ["write","edit"]`（这是轨迹断言的核心价值） |
| 021–025 | 长上下文定位 | `max_input_tokens` 卡住「全文反复读」 |
| 026–030 | 拒绝危险操作 | `must_not_call = ["bash"]` 或 `must_call = []`（安全回归） |

**每条 case 的 `max_input_tokens` 必须来自实测**：先不加该字段跑一次，看打印出的 `tokens=`，再设为该值的 1.5 倍左右。凭空写数字会让门禁要么永远不触发、要么频繁假失败。

- [ ] **Step 8: 写 `just eval` recipe**

在 `justfile` 的 `mutants` 之后追加：

```make
# Agent golden-set eval：评结果 + 评轨迹。
#
# 为什么评轨迹：只评结果的话，「工具选错但碰巧把文件写出来」测不出来。
# 轨迹断言（must_call / must_not_call / max_tool_calls / max_input_tokens）
# 才能抓到过程错误。
#
# 不进 `ci`：真实 API 调用（真钱）+ 分钟级耗时。
# 用法：
#   just eval                 跑全部
#   just eval 007             只跑 id 含 007 的
eval filter="":
    #!/usr/bin/env bash
    set -euo pipefail
    set -a; source ../.env 2>/dev/null || true; set +a
    EVAL_FILTER="{{filter}}" cargo test -p yi-agent --test eval_baseline \
        -- --ignored --nocapture
```

- [ ] **Step 9: 全量跑一遍**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
just eval 2>&1 | tail -40
```

Expected: `跑了 30 条 case，失败 0 条`。**这一跑可能 15-45 分钟**。

对失败的 case，先判断是 case 写得太紧（`max_tool_calls` 卡太死）还是 agent 真有行为问题。前者放宽，后者记录到 Spec 的遗留问题。

- [ ] **Step 10: 反向验证——故意把断言收紧**

```bash
cd yi-agent-rs
# 临时把一条 case 的 max_tool_calls 改成 0
sed -i.bak 's/^max_tool_calls = 4$/max_tool_calls = 0/' crates/yi-agent/tests/eval/cases/002-create-file.toml
just eval 002 2>&1 | grep -E "FAIL|max_tool_calls|超过上限" | head -5
mv crates/yi-agent/tests/eval/cases/002-create-file.toml.bak crates/yi-agent/tests/eval/cases/002-create-file.toml
```

Expected: 看到 `FAIL 002-create-file` 和「工具调用数 N 超过上限 0」。这证明轨迹断言真的会红。

- [ ] **Step 11: 跑 fmt + 提交**

```bash
cd yi-agent-rs
cargo fmt --all
git add justfile crates/yi-agent/Cargo.toml Cargo.lock crates/yi-agent/tests/eval_baseline.rs crates/yi-agent/tests/eval/
git commit -m "test: 建 Agent golden-set eval，评结果也评轨迹

现有 e2e_complex 只断言结果文件，测不出「工具选错但碰巧把文件写出来」。
这里额外断言过程：must_call / must_not_call / max_tool_calls /
max_input_tokens，全部来自 --json 的 JSONL 事件流。

无需改 core：AgentEvent::ToolCall{id,name,input} / Usage{model, usage:
TokenUsage{input_tokens}} 已提供所需全部信息，parse_events 也已能解析。

30 条 case 分六类：单文件读写改、多文件重构、测试失败修复、不该动文件的
问答、长上下文定位、拒绝危险操作。max_input_tokens 必须实测得出（先跑一次
看 tokens= 再设 1.5 倍），凭空写数字会让门禁要么不触发要么频繁假失败。

只进 just eval 与 nightly，不进 just ci：真钱 + 15-45 分钟。

runner 逻辑（extract_trace / check_trajectory / load_cases）与 agent 调用
分离，前者可单测。"
```

---

## Task 6: nightly 定时任务 + cargo audit 进 ci

**Files:**
- Create: `yi-agent-rs/.cargo/audit.toml`
- Modify: `yi-agent-rs/justfile`（加 `audit` recipe 并挂进 `ci`）
- Create: `.github/workflows/nightly.yml`

**Interfaces:**
- Consumes: Task 2 的 `ci-patch`、Task 3 的 `mutants`、Task 5 的 `eval`、现有的 `test-real-all`
- Produces: `just audit`；nightly workflow。

- [ ] **Step 1: 先看当前 audit 输出**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
cargo audit 2>&1 | tail -30
```

Expected: 约 67s，报 30 条（19 个 crate）。

- [ ] **Step 2: 写豁免清单**

按 Spec 的 D1 分类，把「无法修」的录入 `yi-agent-rs/.cargo/audit.toml`：

```toml
# cargo audit 豁免清单。
#
# 只录「确实修不了」的条目，每条必须写明理由与复查条件。
# 能修的（cargo update 即可的 9 个 crate）不录——它们应该被真的修掉，
# 录进这里等于把门禁关掉。
#
# 分类依据见 docs/superpowers/specs/2026-11-02-quality-hardening-wave2-design.md 的 D1。

[advisories]
ignore = [
    # 来自 html2md 0.1.1 的传递依赖，上游已多年未更新，无法在本仓库修。
    # 复查条件：html2md 发布新版本把 regex 提到 1.x 时立即移除。
    "RUSTSEC-2022-0013",  # regex 0.2.11: 未维护
]

# 以下为 unmaintained 标记（非 CVE），cargo audit 会把它们算作 warning。
# 用 warnings = "deny" 会让门禁因这些不可修的条目永远红，故保持默认。
[output]
deny = ["unmaintained"]
```

> **实施时必须核对实际 RUSTSEC 编号**：上面 `RUSTSEC-2022-0013` 是示意，Step 1 的输出里会给出真实编号。把 Step 1 报出的「无法修」类条目逐个录入，**每条都要有一行注释说明复查条件**。不要图省事写 `ignore = ["*"]`。

- [ ] **Step 3: 验证豁免后 audit 变绿**

```bash
cd yi-agent-rs
cargo audit --deny warnings 2>&1 | tail -10
echo "EXIT=$?"
```

Expected: `EXIT=0`。若仍失败，说明还有「能修但没修」的条目——那些属于 D1 第一类，**本 Task 不改 lock**（约束见 Global Constraints），应把它们也临时录进 ignore 并标注「待 cargo update 后移除」。

- [ ] **Step 4: 写 `audit` recipe 并挂进 `ci`**

在 `justfile` 的 `ci:` 之前加：

```make
# 依赖漏洞审计。实测约 67s，不需要编译，所以进 `ci`（秒级门禁挡住新增）。
#
# 为什么不只放 nightly：新披露的 CVE 针对的是**没改动过的依赖**，
# 等开发者触碰相关代码才暴露是不可接受的。
# nightly 里再跑一次，用于捕获「依赖没变但新 CVE 被披露」。
audit:
    cargo audit
```

把 `ci:` 的依赖列表末尾加 `audit`：

```make
ci: fmt-check lint test build bindings-check secret-check npm-version-check ios-version-check release-env-check tag-gate-check build-env-check model-setup-docs-check desktop-target-dir-check rebuild-install-check audit
    @echo "CI passed"
```

- [ ] **Step 5: 验证 `just ci` 仍全绿（现在 15 道门禁）**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin"
cd yi-agent-rs
just ci 2>&1 | tail -10
echo "EXIT=$?"
```

Expected: `CI passed`，`EXIT=0`，总耗时增加约 67s。

- [ ] **Step 6: 更正 CLAUDE.md 的门禁数量**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
grep -n "14 道\|共 14 项" CLAUDE.md
```

把「涵盖 14 道门禁」改为「涵盖 15 道门禁」，把「共 14 项依赖」改为「共 15 项依赖」，并在列举里补上 `audit`。

- [ ] **Step 7: 写 nightly workflow**

`.github/workflows/nightly.yml`：

```yaml
name: Nightly

# 定时任务：覆盖「代码没改但外部世界变了」的失效模式。
# 与 ci.yml 的分工：ci.yml 管「这次提交有没有问题」，这里管「时间带来的问题」——
# 新披露的 CVE、真实 LLM 供应商行为漂移、变异测试的渐进退化。
on:
  schedule:
    # UTC 18:00 = 北京时间 02:00，避开白天开发时段
    - cron: "0 18 * * *"
  workflow_dispatch:  # 手动触发，用于验证 workflow 本身

jobs:
  audit:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: Install cargo-audit
        uses: taiki-e/install-action@v2
        with:
          tool: cargo-audit
      - name: Audit
        working-directory: yi-agent-rs
        run: cargo audit

  mutants:
    runs-on: ubuntu-latest
    timeout-minutes: 40
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: yi-agent-rs
      - name: Install cargo-mutants
        uses: taiki-e/install-action@v2
        with:
          tool: cargo-mutants
      - name: Install just
        uses: extractions/setup-just@v2
      - name: Mutants
        working-directory: yi-agent-rs
        run: just mutants

  real-llm:
    # 真实 API 调用。跑在 GitHub 云端 runner，不占局域网 Mac mini。
    runs-on: ubuntu-latest
    timeout-minutes: 90
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: yi-agent-rs
      - name: Install nextest
        uses: taiki-e/install-action@v2
        with:
          tool: nextest@0.9.150
      - name: Install just
        uses: extractions/setup-just@v2
      # 配置源是 models.json，不是环境变量。见下方「为什么不能直接注入 API key」。
      # export-model-env.sh 从 <instance_home>/models.json 的默认条目读取
      # provider / model / api_url / api_key，导出 MODEL_API_KEY 三件套。
      - name: Write models.json from secrets
        working-directory: yi-agent-rs
        env:
          MODEL_API_KEY: ${{ secrets.MODEL_API_KEY }}
          MODEL_API_URL: ${{ secrets.MODEL_API_URL }}
          MODEL: ${{ secrets.MODEL }}
          PROVIDER: ${{ secrets.PROVIDER }}
        run: |
          mkdir -p "$HOME/.yi-agent"
          python3 - <<'PYEOF'
          import json, os
          entry = {
              "name": "nightly",
              "provider": os.environ["PROVIDER"],
              "model": os.environ["MODEL"],
              "api_url": os.environ["MODEL_API_URL"],
              "api_key": os.environ["MODEL_API_KEY"],
          }
          doc = {"default_model": "nightly", "models": [entry]}
          with open(os.path.expanduser("~/.yi-agent/models.json"), "w") as fh:
              json.dump(doc, fh)
          PYEOF
          echo "models.json 已生成（内容不回显）"
      - name: Real LLM tests
        working-directory: yi-agent-rs
        run: just test-real-all
      - name: Golden-set eval
        working-directory: yi-agent-rs
        run: |
          eval "$(bash scripts/export-model-env.sh)"
          if [ -z "${MODEL_API_KEY:-}" ]; then
            echo "SKIPPED: models.json 没有可用条目"; exit 0
          fi
          cargo test -p yi-agent --test eval_baseline -- --ignored --nocapture
```

> **为什么不能直接 `env: ANTHROPIC_API_KEY: ${{ secrets... }}`——这是实测确认的坑**：
>
> 1. real-llm 系 recipe（`test-real-llm` / `test-real-e2e` / …）**第一步就是 `unset ANTHROPIC_API_KEY OPENAI_API_KEY MODEL_API_KEY MODEL_API_URL YI_AGENT_PROVIDER YI_AGENT_MODEL`**（`justfile:130` 附近）。注释写明理由是「清除 shell 环境变量污染，让 models.json 成为唯一配置源」。所以从 workflow 注入的环境变量会被立刻清掉，测试以「SKIPPED」静默通过——看起来绿了，其实一条都没跑。
> 2. `export-model-env.sh` 只从 `<instance_home>/models.json` 读配置，`instance_home` 由 `YI_AGENT_HOME` 或 `$HOME/.yi-agent` 决定。CI 里要先把 models.json 写出来。
> 3. 通用键是 `MODEL_API_KEY`（外加 `YI_AGENT_PROVIDER` / `YI_AGENT_MODEL` / `MODEL_API_URL` 三件套），不是各家专属的 `ANTHROPIC_API_KEY`。`tests/common/mod.rs:159` 的 `skip_if_no_key` 查的也是 `MODEL_API_KEY`。
>
> **必需 secrets**：`MODEL_API_KEY`、`MODEL_API_URL`、`MODEL`、`PROVIDER`。缺任何一个该 job 会因 python KeyError 失败，而不是静默跳过——这是有意的，避免「配错了还以为在跑」。
>
> **若你的 CI 环境不方便写 models.json**，替代方案是在 workflow 里直接 `export MODEL_API_KEY=... MODEL_API_URL=...` 然后调 `cargo test` 而不调 just recipe（绕过 unset）。但那会与本地路径分叉，不推荐。

- [ ] **Step 8: 验证 workflow 语法**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/nightly.yml')); print('YAML OK')"
```

Expected: `YAML OK`。

- [ ] **Step 9: 手动触发一次验证**

推送分支后：

```bash
gh workflow run nightly.yml --ref feat/quality-wave2
sleep 10
gh run list --workflow=nightly.yml --limit 3
```

Expected: 看到一次 `workflow_dispatch` 触发的运行。**若 secrets 未配置，`real-llm` job 会失败**——这属于仓库配置问题，不是 workflow 缺陷，记录后继续。

- [ ] **Step 10: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
cargo fmt --all --manifest-path yi-agent-rs/Cargo.toml
git add yi-agent-rs/justfile yi-agent-rs/.cargo/audit.toml .github/workflows/nightly.yml CLAUDE.md
git commit -m "ci: 加 nightly 定时任务，cargo audit 提前进 just ci

audit 进 ci 的理由：实测 67s、不需编译，而新披露的 CVE 针对的是没改动过的
依赖——等开发者触碰相关代码才暴露不可接受。nightly 里再跑一次，捕获
「依赖没变但新 CVE 被披露」。

nightly 三个 job：
- audit（67s）
- mutants（8-10 分钟，三处落点，比对 mutants-baseline）
- real-llm（真 LLM 测试 + golden-set eval，跑云端 runner 不占局域网 Mac mini）

挂 GitHub 而非 GitLab：GitLab runner 是局域网里的 Mac mini，夜间跑真 LLM
测试会占用开发机。

豁免清单只录「确实修不了」的条目，每条带复查条件。能被 cargo update 修掉的
9 个 crate 不录——录进去等于把门禁关掉，它们应该在 cargo update 那一波真修。

CLAUDE.md 的门禁数量同步改成 15。"
```

---

## 自查结果

**1. Spec 覆盖检查**

| Spec 章节 | 对应 Task | 状态 |
|---|---|---|
| C1 patch coverage | Task 2 | 覆盖 |
| C2 变异测试（含两个前置障碍） | Task 3 Step 1–7 | 覆盖 |
| C2 门禁语义（退出码 2/3 区分） | Task 3 Step 8 的 `run_one` 函数 | 覆盖 |
| C3 属性测试（三条修正后性质） | Task 4 | 覆盖 |
| C4 golden-set eval | Task 5 | 覆盖 |
| C5 nightly + audit 进 ci | Task 6 | 覆盖 |
| D5 的 12 个存活变异体 | Task 1 | 覆盖 |
| D1 的豁免清单 | Task 6 Step 2 | 覆盖（仅「无法修」类） |
| 验证方式（含反向验证） | Task 1 Step 10、Task 2 Step 4–6、Task 3 Step 10、Task 5 Step 10 | 覆盖 |
| 非目标（不碰 quick-xml/zip、不做 CI 分片、不引入 insta、不拆巨型文件） | Global Constraints + 无对应 Task | 明确排除 |

**2. 占位符扫描**：已检查，无 TBD / TODO / "Similar to Task N" / "add appropriate error handling" / 空泛的「补充错误处理」类措辞。60 个 Step 全部含可执行命令或可粘贴代码。

**2.5 写 Plan 时实测推翻了 4 个原设计假设**（这些如果没查出来，执行者会直接卡住）

| # | 原假设 | 实测结论 | 修正 |
|---|---|---|---|
| 1 | 属性测试放 `tests/proptest_tui.rs` | `yi-agent` 是**纯 binary crate**，无 `lib.rs`，`tui` 在 `main.rs:13` 是私有 `mod`。集成测试访问不到 → **编译不过** | 改为写进各源文件的 `mod tests`；已实测 nextest 采集 bin 单测（`yi-agent::bin/yi-agent tui::wrap::tests::*`） |
| 2 | nightly 注入 `ANTHROPIC_API_KEY` | real-llm 系 recipe **第一步就 `unset`** 所有 key（`justfile:130`），注入会被清掉 → 测试静默 SKIPPED | 改为写 `models.json` + `export-model-env.sh`；通用键是 `MODEL_API_KEY` 不是各家专属名 |
| 3 | wrap 的「每行 ≤ width」普遍成立 | 实测反例：prefix 比 width 宽时 11 行全部超宽（`wrap.rs:52` 的 `.max(1)`，是有意降级） | 性质加前置条件「prefix 宽度 < width」 |
| 4 | slash 的「`from_name(n).name()==n` 对所有 `all()` 元素成立」 | 恒真废话（`all()` 里就是规范名，`find` 直接命中）。且 `from_name("kanban")→Kanban` 而 `Kanban.name()=="superpowers-kanban"` | 改为「规范名往返」+「别名收敛」两条真不变量 |

**2.6 Task 1 的测试代码已真实验证**（不是纸上推演）

把 Task 1 的 5 段测试原样贴进 `permission.rs` 的 `mod tests` 后实测：

```
cargo test -p yi-agent-core --lib permission
→ test result: ok. 65 passed; 0 failed

cargo mutants --file crates/yi-agent-core/src/permission.rs --timeout 60 -j 8
→ 50 mutants tested in 2m: 0 missed, 44 caught, 5 unviable, 1 timeouts
```

12 个存活体**全部杀死**，caught 从 32 升到 44。验证后已回滚，Plan 分支不留源码改动。

**3. 类型与命名一致性**

- `PermissionsConfig` / `ToolLevelConfig` / `Decision` / `PermissionKind` / `CheckResult` / `YoloSwitch` / `BlocklistFn` 全部来自 `permission.rs` 现有定义
- `checker_with`（`:463`）、`bash_input`（`:479`）是现有测试辅助函数，Task 1 复用
- `parse_events` / `event_variant` / `run_command_with_timeout` / `run_agent_with_timeout` / `skip_if_no_key` / `yi_agent_bin` 全部来自 `tests/common/mod.rs` 现有导出
- `Trace` / `Case` / `Expect` / `TrajectoryExpect` 在 Task 5 内定义并使用，与 `eval_baseline.rs` 的调用一致
- 三个 recipe 名 `ci-patch` / `mutants` / `eval` / `audit` 在 Task 2、3、5、6 里拼写一致

**4. 已知的实施风险（非 Plan 缺陷，需执行者知情）**

- `rpc_tests.rs` 的 `a_completed_item_never_trails_its_own_buffered_delta` 仍是 flaky（另一进程在修）。Task 2 的 `ci-patch` 需要全量测试全绿，在 flaky 未归零前会偶发挡住。**这不是 Plan 的错误**，遇到时重试即可，不要在 Plan 里加 `--no-fail-fast` 之类绕过。
- Task 5 的 30 条 case 需要真实 API key 和 15-45 分钟，调试成本高。建议先用 `EVAL_FILTER` 单条跑通再全量。
- Task 3 Step 6 若 `path_util` 的 missed 超过 10 个，按 Spec 的要求应回退评估是否换落点（备选 `core/src/autonomy.rs`），而不是硬补 10+ 个测试。
