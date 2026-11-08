# 统一测试分级（范围 × 保真度）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把仓库里五套并行的测试入口整合成两个正交轴（范围 `L0-L3`、保真度 `mock/proc/live`），消除 CI 里重复跑一遍的 263 个真进程测试，并修掉两处假绿。

**Architecture:** 范围轴（`check-fast` / `test-crate` / `test-affected` / `test`）已经存在且工作正常，本计划**不改动**它们。改动集中在保真度轴：把 4 个 `test-real-*` recipe 收敛成唯一的 `just test-live [suite]`，迁移到 nextest 的独立 `[profile.live]`，并用 `binary_id` 限定的 `default-filter` 防止 `--run-ignored all` 误拉起真实 API 调用。

**Tech Stack:** Rust workspace、nextest 0.9.150、just、bash、GitHub Actions YAML。

## Global Constraints

- 开发必须在 worktree 中完成，禁止在 `main` 上直接改代码。
- 若有其他会话在跑 cargo/nextest/rustc，等它们结束再跑编译任务。检查：
  `pgrep -x cargo; pgrep -x cargo-nextest; pgrep -x rustc`
- `nextest.toml` 里 `period` 必须带单位字符串（如 `"300s"`），`terminate-after` 必须是整数。
  写错会在解析阶段直接失败。
- `default-filter` 里的排除项**必须**用 `binary_id(...)` 限定作用域，且**每个二进制
  用各自的测试名前缀**。live 测试命名不统一：`real_anthropic_`/`real_openai_`
  （`real_integration`）、`e2e_`（`e2e_real`）、`complex_`（`e2e_complex`）、
  `real_subagent_`（`subagent_real_e2e`）、`changing_models_json_`
  （`model_catalog_live_reload_e2e`）。写成统一的 `^real_` 只挡住第一个二进制。
- 无可用 key 时 live 入口必须打印 `SKIPPED: ...` 并 `exit 0`，沿用
  `scripts/export-model-env.sh` 既有约定。
- 不修改任何测试断言，不修 `real_openai_*` 与 `e2e_auto_compact_triggers` 两个已知失败。
- 提交前 `cargo fmt --all`；commit 用 conventional commits，不写 `Co-Authored-By`。
- live 测试总数固定为 **18**（6 `real_integration` + 5 `e2e_real` + 4 `e2e_complex`
  + 2 `subagent_real_e2e` + 1 `model_catalog_live_reload_e2e`）。

---

### Task 1: nextest 配置 —— default 排除 live + 独立 live profile

**Files:**
- Modify: `yi-agent-rs/.config/nextest.toml`

**Interfaces:**
- Consumes: 无（第一个任务）
- Produces: `[profile.live]`（供 Task 2 的 `--profile live` 使用）、
  `[test-groups.live-api]`（`max-threads = 3`）。
  default profile 的 `default-filter` 变为三行以上的多行字符串。

- [x] **Step 1: 先记录改动前的基线数字**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"
cargo nextest list --all-features --workspace 2>/dev/null | grep -c '::'
```
Expected: `6020`（数量随仓库演化会变，记下实际值，后面要用）。

- [x] **Step 2: 写一条会失败的检查（证明当前确实没有 live profile）**

```bash
cd yi-agent-rs
cargo nextest list --all-features --profile live -p yi-agent --test e2e_real \
  --run-ignored only 2>&1 | tail -3
```
Expected: 报错 `profile live not found`（或类似）。这证明 profile 尚不存在。

- [x] **Step 3: 修改 `yi-agent-rs/.config/nextest.toml`**

把现有的 `default-filter = "not test(runtime_process_helper)"` 一行替换为下面的多行
版本，并在文件末尾追加 live profile 与 test-group。**已有的
`slow-timeout` / `fail-fast` / `[profile.ci]` / `[test-groups.spawn-heavy]` /
`[[profile.default.overrides]]` 全部保留不动。**

```toml
[profile.default]
default-filter = """
not test(runtime_process_helper)
and not (binary_id(yi-agent-llm::real_integration) and test(/^real_(anthropic|openai)_/))
and not (binary_id(yi-agent::e2e_real) and test(/^e2e_/))
and not (binary_id(yi-agent::e2e_complex) and test(/^complex_/))
and not (binary_id(yi-agent::subagent_real_e2e) and test(/^real_subagent_/))
and not (binary_id(yi-agent::model_catalog_live_reload_e2e) and test(/^changing_models_json_/))
"""
```

**五个前缀各不相同，这不是笔误。** live 测试的实际命名见下表——写错前缀会导致
排除无效，而 `just test` 表面上"通过"（因为 live 默认本就不跑），直到有人用
`--run-ignored all` 才暴露：

| 二进制 | live 前缀 | 数量 | 该二进制 normal 数 |
|---|---|---|---|
| `yi-agent-llm::real_integration` | `real_anthropic_` / `real_openai_` | 6 | 0 |
| `yi-agent::e2e_real` | `e2e_` | 5 | 3 |
| `yi-agent::e2e_complex` | `complex_` | 4 | 3 |
| `yi-agent::subagent_real_e2e` | `real_subagent_` | 2 | 6 |
| `yi-agent::model_catalog_live_reload_e2e` | `changing_models_json_` | 1 | 3 |

文件末尾追加：

```toml
# ---------------------------------------------------------------------------
# live：真实 LLM API
# ---------------------------------------------------------------------------
#
# 为什么需要独立 profile，而不是只给 default 加 overrides：
#
# 1. 超时。default 的 slow-timeout 是 60s x 3 = 180s 就终止，而 live 测试自身
#    预算是 300s（E2E_TIMEOUT / COMPLEX_TIMEOUT）。不抬预算会把合法测试误杀成
#    「不稳定」。这里 300s x 2 = 600s，是自身预算的 2 倍。
#
# 2. default-filter 必须分 profile（决定性原因）。上面 default profile 排除了
#    live 测试，好让 `--run-ignored all` 不会误跑真实 API。但实测发现：一旦
#    default 排除了它们，`just test-live` 用 `--run-ignored only` 会拿到 0 个
#    测试，`-E` 也覆盖不了。所以必须有一个不排除 live 的 profile。
#
# 实测（2026-11-08 / nextest 0.9.150）：
#   default profile 排除 live          -> --run-ignored only 得 0 个
#   default 排除 + -E 'test(/^real_/)' -> 仍为 0 个
#   独立 [profile.live] 不排除        -> 得 18 个（6+5+4+2+1）

[profile.live]
inherits = "default"
default-filter = "not test(runtime_process_helper)"

[test-groups.live-api]
# 并发 3：实测 e2e_real 串行 90.01s -> 并发 3 为 38.17s，且通过/失败集合与
# 串行完全一致（两处失败串行也复现，故与并发无关）。真实 API 有速率限制，
# 不要再往上抬。
max-threads = 3

[[profile.live.overrides]]
filter = 'binary_id(yi-agent-llm::real_integration) + binary_id(yi-agent::e2e_real) + binary_id(yi-agent::e2e_complex) + binary_id(yi-agent::subagent_real_e2e) + binary_id(yi-agent::model_catalog_live_reload_e2e)'
test-group = 'live-api'
slow-timeout = { period = "300s", terminate-after = 2 }
```

- [x] **Step 4: 验证 live profile 能取到 18 个 live 测试**

```bash
cd yi-agent-rs
tot=0
for spec in "yi-agent-llm real_integration" "yi-agent e2e_real" "yi-agent e2e_complex" \
            "yi-agent subagent_real_e2e" "yi-agent model_catalog_live_reload_e2e"; do
  set -- $spec
  n=$(cargo nextest list --all-features --profile live -p $1 --test $2 --run-ignored only 2>/dev/null | grep -c '::')
  printf '  %-40s %s\n' "$2" "$n"
  tot=$((tot+n))
done
echo "合计: $tot"
```
Expected:
```
  real_integration                         6
  e2e_real                                 5
  e2e_complex                              4
  subagent_real_e2e                        2
  model_catalog_live_reload_e2e            1
合计: 18
```

- [x] **Step 5: 验证 default profile 不再包含 live 测试**

```bash
cd yi-agent-rs
# 应该输出 0（default profile 下 live 测试被排除）
cargo nextest list --all-features --workspace 2>/dev/null | grep -cE 'e2e_(auto_compact|simple_text|tool_use|error_no_api)|complex_(bug_fix|data_transform|personal_web|python_script)|changing_models_json|real_(anthropic|openai)_|real_subagent_'
```
Expected: `0`（实测确认）

- [x] **Step 6: 验证排除项没有误伤正常测试**

```bash
cd yi-agent-rs
before=6020   # 换成 Step 1 记下的实际值
after=$(cargo nextest list --all-features --workspace 2>/dev/null | grep -c '::')
echo "before=$before after=$after diff=$((before-after))"
# 逐个验证五个二进制的 normal 测试数没变（0 / 3 / 3 / 6 / 3）
for spec in "yi-agent-llm real_integration" "yi-agent e2e_real" "yi-agent e2e_complex" \
            "yi-agent subagent_real_e2e" "yi-agent model_catalog_live_reload_e2e"; do
  set -- $spec
  printf '  %-40s ' "$2"
  cargo nextest list --all-features -p $1 --test $2 2>/dev/null | grep -c '::'
done
```
Expected: `0 / 3 / 3 / 6 / 3`，且全量 `diff=0`。

> 若 `diff` 不为 0，说明排除项误伤了正常测试，必须停下来查。
> 若某个二进制的 normal 数变化，说明该条前缀写错，排除到了不该排的东西。

- [x] **Step 7: 验证 `--run-ignored all` 下 live 被挡住**

```bash
cd yi-agent-rs
# 加 filter 后：18 个 live 被挡住，只应剩各二进制的正常测试
tot=0
for spec in "yi-agent-llm real_integration" "yi-agent e2e_real" "yi-agent e2e_complex" \
            "yi-agent subagent_real_e2e" "yi-agent model_catalog_live_reload_e2e"; do
  set -- $spec
  n=$(cargo nextest list --all-features -p $1 --test $2 --run-ignored all 2>/dev/null | grep -c '::')
  tot=$((tot+n))
done
echo "合计: $tot"
```
Expected: 各二进制为 `0 / 3 / 3 / 6 / 3`，**合计 15**。
未加 filter 时同一命令合计为 **33**，差值正好 18（= live 测试数）。
关键判据两条：合计必须等于 15，且不能是 0（正常测试仍在）。

- [x] **Step 8: 确认 `just test` 仍能正常工作（不跑 live）**

```bash
cd yi-agent-rs
cargo nextest list --all-features --workspace --profile ci 2>/dev/null | grep -c '::'
```
Expected: 等于 Step 1 的基线值（`--profile ci` 继承 default，同样排除 live）。

- [x] **Step 9: Commit**

```bash
git add yi-agent-rs/.config/nextest.toml
git commit -m "test: split live tests into their own nextest profile

default profile now excludes the 18 real-LLM tests, so a stray
--run-ignored all cannot fire real API calls and cost money.

Each exclusion must be binary_id-scoped with that binary's own test-name
prefix. The prefixes differ: real_anthropic_/real_openai_ in
real_integration, e2e_ in e2e_real, complex_ in e2e_complex,
real_subagent_ in subagent_real_e2e, changing_models_json_ in
model_catalog_live_reload_e2e. A single generic ^real_ pattern only
covers the first of the five: measured, --run-ignored all drops from 33
to 25 instead of the intended 15.

A separate [profile.live] is required rather than optional, because
excluding from default-filter makes --run-ignored only return zero
tests for test-live, and -E cannot override that. Measured: default
yields 0, live profile yields 18."
```

---

### Task 2: `just test-live [suite]` 入口

**Files:**
- Create: `yi-agent-rs/scripts/test-live.sh`
- Modify: `yi-agent-rs/justfile:434-500`（`# === 真实 LLM 测试 ===` 到 `test-real-all` 整段）

**Interfaces:**
- Consumes: Task 1 的 `[profile.live]`、`[test-groups.live-api]`。
- Produces: `just test-live [suite]`，suite 取值 `llm|e2e|complex|subagent|catalog|`
  （空 = 全部）。`scripts/test-live.sh` 接受一个参数：suite 名或空。

- [x] **Step 1: 写脚本并先验证它在无 key 时的行为（这是本任务的核心验收点）**

创建 `yi-agent-rs/scripts/test-live.sh`：

```bash
#!/usr/bin/env bash
# 真实 LLM 测试的唯一入口。
#
# 两个职责：
#   1. 统一 key 检查。历史上 test-real-subagent 完全不检查 key，导致无 key 时
#      跑 0 个测试然后 exit 0 —— 报告写「通过」但什么都没验证。这里检查三种
#      key 的并集，任何一个存在就放行。
#   2. 把 suite 名映射到 (package, test binary)。
#
# 无可用 key 时打印 SKIPPED 并 exit 0：缺工具是环境问题，不伪装成测试失败
# （沿用 scripts/export-model-env.sh 的既有约定）。
set -euo pipefail

suite="${1:-}"

# suite -> "package test_binary"
describe_suite() {
    case "$1" in
        llm)      echo "yi-agent-llm real_integration" ;;
        e2e)      echo "yi-agent e2e_real" ;;
        complex)  echo "yi-agent e2e_complex" ;;
        subagent) echo "yi-agent subagent_real_e2e" ;;
        catalog)  echo "yi-agent model_catalog_live_reload_e2e" ;;
        *)        return 1 ;;
    esac
}

ALL_SUITES="llm e2e complex subagent catalog"

if [ -n "$suite" ]; then
    if ! describe_suite "$suite" >/dev/null; then
        echo "error: unknown suite '$suite'" >&2
        echo "valid suites: $ALL_SUITES (omit the argument to run all)" >&2
        exit 1
    fi
    SUITES="$suite"
else
    SUITES="$ALL_SUITES"
fi

# 清除 shell 环境变量污染（如 ~/.zshrc 里的全局 key），让 models.json 成为唯一配置源
unset ANTHROPIC_API_KEY OPENAI_API_KEY MODEL_API_KEY MODEL_API_URL YI_AGENT_PROVIDER YI_AGENT_MODEL
eval "$(bash scripts/export-model-env.sh)"

# 统一 key 检查：三者的并集。任一存在即可跑。
if [ -z "${ANTHROPIC_API_KEY:-}" ] && [ -z "${OPENAI_API_KEY:-}" ] && [ -z "${MODEL_API_KEY:-}" ]; then
    echo "SKIPPED: no provider API key configured (models.json has no usable default entry)"
    exit 0
fi

# 通用键优先：models.json 给的是 MODEL_API_KEY + provider/model 三件套，
# 而 provider 层测试认的是各家专属变量名。
if [ -n "${MODEL_API_KEY:-}" ] && [ -n "${YI_AGENT_PROVIDER:-}" ]; then
    case "$YI_AGENT_PROVIDER" in
        anthropic) export ANTHROPIC_API_KEY="$MODEL_API_KEY" ;;
        openai)    export OPENAI_API_KEY="$MODEL_API_KEY" ;;
    esac
fi

for s in $SUITES; do
    read -r pkg bin <<<"$(describe_suite "$s")"
    echo "=== live suite: $s ($pkg::$bin) ==="
    cargo nextest run --all-features --profile live --run-ignored only \
        -p "$pkg" --test "$bin"
done
```

赋可执行权限并**先测无 key 路径**：

```bash
cd yi-agent-rs
chmod +x scripts/test-live.sh
env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u MODEL_API_KEY \
    YI_AGENT_HOME=/tmp/nonexistent-yi-home-$$ \
    bash scripts/test-live.sh llm
```
Expected: 打印 `SKIPPED: no provider API key configured ...` 且 `exit 0`。
用 `echo $?` 确认返回码是 0。

> 关键：**不能出现“无输出、跑 0 个测试、rc=0”**。必须有 `SKIPPED` 字样，
> 这是病因 4 的形态（假绿）与本设计的分界。

- [x] **Step 2: 验证未知 suite 名报错**

```bash
cd yi-agent-rs
bash scripts/test-live.sh nosuchsuite; echo "rc=$?"
```
Expected: 打印 `error: unknown suite 'nosuchsuite'` 与合法取值，`rc=1`。

- [x] **Step 3: 验证 5 个 suite 全部能映射（覆盖病因 5）**

```bash
cd yi-agent-rs
for s in llm e2e complex subagent catalog; do
  echo "--- $s ---"
  env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u MODEL_API_KEY \
      YI_AGENT_HOME=/tmp/nonexistent-yi-home-$$ \
      bash scripts/test-live.sh "$s" 2>&1 | head -2
done
```
Expected: 每个都打印 `SKIPPED`（无 key 环境），且**五个都不报 unknown suite**。
特别是 `catalog` —— 它历史上没有任何入口（病因 5）。

- [x] **Step 4: 替换 justfile 的旧段落**

删除 `yi-agent-rs/justfile` 里 `# === 真实 LLM 测试(需 API key,默认跳过) ===` 到
`test-real-all: ...` / `@echo "All real LLM tests passed"` 之间的**全部内容**
（即 `test-real-llm`、`test-real-e2e`、`test-real-complex`、`test-real-subagent`、
`test-real-all` 五个 recipe 及其注释），替换为：

```just
# === 真实 LLM 测试（保真度轴：live） ===
#
# 这是唯一的 live 入口，取代旧的 test-real-llm / -e2e / -complex / -subagent /
# -all 五个 recipe。它们有三套不一致的 key 检查逻辑，其中 test-real-subagent
# 完全不检查，无 key 时会跑 0 个测试然后成功（假绿）；model_catalog_live_reload
# 则完全没有入口。
#
# 配置源：`~/.yi-agent/models.json` 的默认条目（见 scripts/export-model-env.sh）。
# 无可用 key 时打印 SKIPPED 并 exit 0。
#
# 用法：
#   just test-live            跑全部 5 个 suite（18 个测试）
#   just test-live llm        provider 层（real_integration）
#   just test-live e2e        端到端（e2e_real）
#   just test-live complex    复杂 one-shot（e2e_complex）
#   just test-live subagent   subagent 回归（subagent_real_e2e）
#   just test-live catalog    模型目录热加载（model_catalog_live_reload_e2e）
test-live suite="":
    bash scripts/test-live.sh "{{suite}}"
```

- [x] **Step 5: 验证新入口可用，旧名字已消失**

```bash
cd yi-agent-rs
just --list 2>/dev/null | grep -E 'test-live|test-real'
```
Expected: 只有 `test-live`，**没有**任何 `test-real-*`。

```bash
cd yi-agent-rs
env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u MODEL_API_KEY \
    YI_AGENT_HOME=/tmp/nonexistent-yi-home-$$ just test-live 2>&1 | head -3
```
Expected: `SKIPPED: ...`，无报错。

- [x] **Step 6: 验证无参时五个 suite 都被调度**

```bash
cd yi-agent-rs
env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u MODEL_API_KEY \
    YI_AGENT_HOME=/tmp/nonexistent-yi-home-$$ bash scripts/test-live.sh 2>&1 | grep -c 'live suite:'
```
Expected: `0` —— 因为无 key 时在进入循环前就 `SKIPPED` 了。

为了验证循环本身，用一个假 key 跑（不必真实有效，只看调度）：

```bash
cd yi-agent-rs
# 用不存在的 key 只为让流程走到循环；测试会因 API 调用失败，但我们只数调度次数
MODEL_API_KEY=dummy-for-scheduling-test YI_AGENT_PROVIDER=anthropic \
  bash scripts/test-live.sh 2>&1 | grep -c 'live suite:'
```
Expected: `5`（五个 suite 都被调度到）。失败是预期的（假 key），不追究。

- [x] **Step 7: Commit**

```bash
git add yi-agent-rs/scripts/test-live.sh yi-agent-rs/justfile
git commit -m "feat(test): replace the four test-real-* recipes with just test-live

The old recipes carried three different key checks. test-real-subagent had
none at all, so with no key it ran zero tests, exited 0, and reported
success. model_catalog_live_reload_e2e had no entrypoint anywhere.

One script, one key check (union of ANTHROPIC/OPENAI/MODEL), five suites.
Runs under nextest --profile live so the 300s budget is not cut short at
180s and concurrency stays at 3."
```

---

### Task 3: 删除 CI macOS job 的重复步骤

**Files:**
- Modify: `.github/workflows/ci.yml:150-171`

**Interfaces:**
- Consumes: Task 1 验证的「263 个 proc 测试在 nextest 下全绿」。
- Produces: 无（终端改动）。本地 `just runtime-lifecycle-check` 保留不变。

- [x] **Step 1: 再确认一次 proc 测试在 nextest 下全绿（删 CI 步骤的前提）**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"
cargo nextest run --all-features --profile ci \
  -p yi-agent -p yi-agent-supervisors -p yi-agent-store -p yi-agent-subagent -p yi-agent-tools \
  --test managed_runtime_lifecycle --test managed_roles --test frontend_runtime_lease \
  --test managed_bringup --test managed_process_groups --test managed_channel \
  --test managed_channel_authority --test managed_attach_authority --test client_reclamation \
  --test execution_admission --test guardian_control --test guardian_status \
  --test guardian_handover --test runtime_switch --test runtime_migration \
  --test runtime_scope --test runtime_restart_preview --test executor_isolation \
  --test executor_bridge --test executor_gate --test bringup_policy --test managed_bash \
  2>&1 | tail -5
```
Expected: `263 tests run: 263 passed, 4 skipped`（数字允许小偏差，关键是 **0 failed**）。
若有失败，**停止本任务**并报告 —— 不能删除 CI 步骤。

- [x] **Step 2: 读取当前的 macOS job 尾部**

```bash
sed -n '140,175p' .github/workflows/ci.yml
```
确认待删除的是 `- name: Run shared runtime lifecycle tests` 这一步及其上方的
注释块（当前为 152-171 行附近）。

- [x] **Step 3: 删除该步骤，并修正上方那条错误注释**

把注释块（`# 共享运行时生命周期的真进程验收。` 到
`# 无头环境不声称覆盖。`）连同 `- name: Run shared runtime lifecycle tests` 与
`run: just runtime-lifecycle-check` 一起删除，替换为一条说明为什么不单独跑：

```yaml
      # 真进程生命周期测试已包含在上面的 nextest 步骤里（它们不带 #[ignore]，
      # 所以 `cargo nextest run` 本来就会执行）。2026-11-08 实测：23 个 target
      # 共 263 个测试在 nextest 下 263 passed / 4 skipped，34s。
      #
      # 这里先前有第二步用 `just runtime-lifecycle-check`（cargo test）把它们
      # 又跑一遍，理由写的是「managed_runtime_lifecycle 用 current_exe() +
      # --exact 拉 fixture，nextest 的 --exact 语义不同」。该理由不成立：那个
      # 文件里根本没有 current_exe（只有环境变量门控的 helper），而真正用
      # current_exe() + --exact --ignored 的 managed_channel 在 nextest 下
      # 7/7 通过。删除重复步骤，同一批测试一次运行只跑一遍。
```

- [x] **Step 4: 验证 YAML 合法**

```bash
python3 -c "
import yaml,sys
d=yaml.safe_load(open('.github/workflows/ci.yml'))
jobs=d['jobs']
print('jobs:', sorted(jobs))
steps=[s.get('name','<unnamed>') for s in jobs['macos']['steps']]
print('macos steps:', steps)
assert not any('lifecycle' in (s or '') for s in steps), 'lifecycle step still present'
print('OK: lifecycle step removed')
"
```
Expected: 打印 `OK: lifecycle step removed`。

- [x] **Step 5: 确认本地入口仍在（只是 CI 不调了）**

```bash
cd yi-agent-rs
grep -n '^runtime-lifecycle-check:' justfile
```
Expected: 仍存在（例如 `runtime-lifecycle-check:` 在某行）。

- [x] **Step 6: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: stop running the lifecycle tests twice in the macos job

The macos job ran the 263 process-spawning tests twice: once via nextest
(the targets are not #[ignore]d), then again via
just runtime-lifecycle-check.

The comment justifying the second run said nextest could not handle the
current_exe() + --exact fixture. That is not true: managed_runtime_lifecycle
has no current_exe at all, and managed_channel, which really does use
current_exe() + --exact --ignored, passes 7/7 under nextest. The full set
of 263 passes under nextest in 34s.

The local recipe stays for running them in isolation."
```

---

### Task 4: CLAUDE.md —— 合并两个轴，消除命名冲突

**Files:**
- Modify: `CLAUDE.md`（`## 分级测试入口(L0/L1/L2)` 与 `## 真实 LLM 测试`、
  `## 分级测试系统` 三节）

**Interfaces:**
- Consumes: Task 1-2 的最终命令名（`just test-live`）。
- Produces: 无（文档）。

- [x] **Step 1: 定位三节的当前行号**

```bash
grep -n '^## 分级测试入口\|^## 真实 LLM 测试\|^## 分级测试系统\|^## 项目进度维护' CLAUDE.md
```
Expected: 四个行号。`## 分级测试系统` 到 `## 项目进度维护` 之间是本任务要重写的
范围（含 `## 真实 LLM 测试`）。

- [x] **Step 2: 把 `## 分级测试入口(L0/L1/L2)` 整节替换为统一的「两个轴」章节**

```markdown
## 测试分级:两个正交轴

> 只有一套分级概念。旧文档里「Tier 0-3」与「L0/L1/L2」是两个不同维度,
> 已合并到下面这张表。

### 两个轴

| 轴 | 取值 | 决定什么 |
|---|---|---|
| **范围** (scope) | `L0` check / `L1` crate / `L2` affected / `L3` workspace | 跑多少代码 |
| **保真度** (fidelity) | `mock` / `proc` / `live` | 要不要真进程、要不要真 API |

- **mock**:纯内存 / wiremock,无真实进程。
- **proc**:真 fork guardian / daemon / 子进程,等真实 10 秒优雅窗口。263 个测试,
  实测 34 秒。
- **live**:真实 LLM API,花钱、有速率限制。18 个测试,并发 3。

### 用法(照着选,不用记轴)

| 场景 | 命令 |
|---|---|
| 改完看能不能编过 | `just check-fast` |
| 只改了一个 crate | `just test-crate <crate>` |
| 改了几处,想知道牵动谁 | `just test-affected [ref]` |
| 收尾 / 合并前 | `just test` |
| 想验证真实 API 还能通 | `just test-live [suite]` |

- `just test` 覆盖 `L3 + mock+proc`。**永不触发真实 API 调用。**
- `just test-live` 是唯一的 live 入口。suite 取 `llm` / `e2e` / `complex` /
  `subagent` / `catalog`,省略则跑全部 18 个。无 key 时打印 `SKIPPED` 并 exit 0。
- `just test-affected` 默认对比 `$(git merge-base main HEAD)`,**含未提交改动**;
  传 ref 则对比该 ref,例如 `just test-affected origin/main`。
- 只想跑单个测试: `cargo nextest run --all-features -p <crate> -E 'test(<name>)'`。
  跑 live 的单个测试: `cargo nextest run --all-features --profile live
  --run-ignored only -p <crate> --test <binary> -E 'test(<name>)'`。

### 设计原则(改这些命令前必读)

- **分级的意义是「不同场景测不同的东西」,不是把全量测试加速。** L0-L2 各自省掉
  不同成本,都不等价于 `just test`;合并前仍要跑全量。
- **proc 不是独立档位。** 263 个 proc 测试只占全量约 4%(34s / 821s),单独切一层
  的认知成本大于收益。慢的根因是全量编译与 6020 个测试,解法是范围分层。
- **`--all-targets` 不能省(L0)。** 默认不检查 `tests/`,而本仓有 108 个集成测试
  target。实测在 `crates/*/tests/` 放类型错误,不带该 flag 报 0 error(漏检)。
- **`--all-features` 不能省(L1/L2)。** `yi-agent-store` 的
  `foreground-test-support` feature 门控了测试辅助方法,漏掉直接 `error[E0599]`。
- **L2 用 nextest 原生 `rdeps()`,不手工算闭包**,依赖图漂移交给 nextest。
  闭包条数是验收依据: 改 `core` 得 12 个、`relay` 得 3 个、`flags` 得 8 个。
- **L2 默认基准必须是 merge-base 的两点 diff**,不是三点 `main...HEAD`。
- **判据写错不会报错,只会少测。** 映射链上任何失败都必须显式 exit 非零,
  不能退化成「全部 unmapped」后返回 0。
- **判据自测: `just affected-check`**(29 条断言,已进 `just ci`)。
- **`just test` 前的并发预检只警告不阻断。** 看到 WARN 且失败是超时类时,
  **先独占复跑再判断**。
- **live 必须走 `[profile.live]`,不能用 default。** default profile 的
  `default-filter` 排除了 live 测试(防 `--run-ignored all` 误跑真实 API),
  因此 `--run-ignored only` 在 default 下会拿到 0 个测试。`-E` 覆盖不了。
- **`default-filter` 的排除项必须用 `binary_id(...)` 限定。** 裸写
  `not test(/^real_/)` 会误伤 `yi-agent-runtime`、`eval_baseline`、
  `frontend_runtime_lease` 里名称以 `real_` 开头的正常测试。
- **超时预算必须大于测试自身预算。** live 测试自身是 300s
  (`E2E_TIMEOUT`/`COMPLEX_TIMEOUT`),profile 里设 `300s x 2 = 600s`。若设成
  default 的 180s 会把合法测试误杀成「不稳定」。

实现: `yi-agent-rs/scripts/test-tiers.sh`(范围轴)+ `yi-agent-rs/scripts/test-live.sh`
(保真度轴)+ `.config/nextest.toml`(profile 与 test-group)。
设计依据: `docs/superpowers/specs/2026-11-08-unified-test-tiers-design.md`
```

- [x] **Step 3: 删除 `## 真实 LLM 测试` 与 `## 分级测试系统` 两节**

这两节的内容已被 Step 2 的新章节吸收。特别要删掉：

- `## 分级测试系统` 里的「Tier 0 (Mock) / Tier 1 / Tier 2 / Tier 3」表格
- `## 真实 LLM 测试` 里的 `just test-real-llm` / `test-real-e2e` /
  `test-real-complex` / `test-real-all` 命令列表（这些 recipe 已在 Task 2 删除，
  文档若保留会指向不存在的命令）

保留 `## 真实 LLM 测试` 里仍然有效的信息，并入 Step 2 章节之外的一个简短小节：

```markdown
## 真实 LLM 测试的环境要求

- 配置源是 `~/.yi-agent/models.json` 的默认条目（`scripts/export-model-env.sh`
  从中导出变量）。**不要**依赖 shell 里的全局 key——入口会先 `unset` 它们。
- 无可用 key 时所有 live 入口打印 `SKIPPED` 并 exit 0，不算失败。
- CI **不跑** live 测试（避免成本与密钥泄露）。
- 已知失败（与并发无关，串行也复现，属另一条工作线）：`real_openai_*`
  打 `https://api.openai.com` 而实际 provider 走自定义 URL；
  `e2e_auto_compact_triggers` 依赖 token 计数假设。
```

- [x] **Step 4: 验证没有残留的旧命令名**

```bash
grep -n 'test-real-llm\|test-real-e2e\|test-real-complex\|test-real-subagent\|test-real-all' CLAUDE.md
```
Expected: 无输出（exit 1）。若有输出，说明文档还指向已删除的 recipe，必须改掉。

```bash
grep -c '^## 分级' CLAUDE.md
```
Expected: `1`（只有一个分级章节）。

```bash
grep -n 'Tier 0\|Tier 1\|Tier 2\|Tier 3' CLAUDE.md
```
Expected: 无输出（Tier 术语已并入两个轴）。

- [x] **Step 5: 逐条执行文档里的新命令，确认都真实可用**

```bash
cd yi-agent-rs
just --list 2>/dev/null | grep -E 'check-fast|test-crate|test-affected|test-live|^    test '
```
Expected: 五个都出现。

- [x] **Step 6: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: merge the two graded-test chapters into one concept

CLAUDE.md had two sections both called '分级': one by scope (L0/L1/L2),
one by whether real LLM calls were needed (Tier 0-3). They are different
axes, but the shared name forced a 'do not confuse these' warning box.

Now there is one section with two axes and one table of entrypoints. Also
drops the test-real-* command names, which no longer exist after the
previous commit, and records the two live-config traps (must use
--profile live; default-filter exclusions must be binary_id-scoped)."
```

---

### Task 5: 项目进度文档同步

**Files:**
- Modify: `docs/project-management/tooling.md`

**Interfaces:**
- Consumes: Task 1-4 的成果。
- Produces: 无。

- [x] **Step 1: 找到现有的分级测试条目**

```bash
grep -n '分级测试\|test-real\|runtime-lifecycle' docs/project-management/tooling.md
```

- [x] **Step 2: 更新或新增条目**

把现有的「分级测试入口（L0/L1/L2）」条目改为反映两个轴，并新增 live 入口的记录。
条目必须带**可验证的完成判据**（命令或文件位置），不能写主观描述：

```markdown
- [x] 测试分级:两个正交轴(范围 × 保真度)
  - 范围轴: `just check-fast` / `just test-crate <c>` / `just test-affected` /
    `just test`,实现在 `yi-agent-rs/scripts/test-tiers.sh`
  - 保真度轴: `just test-live [suite]`,实现在 `yi-agent-rs/scripts/test-live.sh`,
    suite 覆盖 `llm` / `e2e` / `complex` / `subagent` / `catalog`(共 18 个测试)
  - live 用独立 profile: `.config/nextest.toml` 的 `[profile.live]`,
    并发 3(`[test-groups.live-api]`)、超时 600s
  - default profile 排除 live: `.config/nextest.toml` 的 `default-filter`,
    五个 `binary_id(...) and test(/^real_/)` 排除项
  - 完成判据:
    `cargo nextest list --all-features --profile live --run-ignored only`
    对五个二进制合计 18 个;
    `cargo nextest list --all-features --workspace` 不含任何 live 测试
```

- [x] **Step 3: 更新模块索引表的计数**

```bash
grep -n 'tooling' docs/project-management/README.md
```
按该表既有格式调整「完成 / 总计」计数（新增或修改了多少条就加减多少）。

- [x] **Step 4: 验证文档里的命令可执行**

```bash
cd yi-agent-rs
cargo nextest list --all-features --profile live --run-ignored only \
  -p yi-agent -p yi-agent-llm --test real_integration 2>/dev/null | grep -c '::'
```
Expected: `6`

- [x] **Step 5: Commit**

```bash
git add docs/project-management/tooling.md docs/project-management/README.md
git commit -m "docs: record the unified test tiers in project management"
```

---

## 收尾验证

全部任务完成后，跑一次完整验收：

- [x] `just test-live` 无 key 时打印 `SKIPPED` 且 `exit 0`
- [x] `just test-live nosuchsuite` 报错 `rc=1`
- [x] `cargo nextest list --profile live --run-ignored only` 五个二进制合计 18
- [x] `cargo nextest list --workspace` 在 default profile 下不含 live 测试
- [x] `default-filter` 加排除前后全量测试数差值为 0（不误伤正常测试）
- [x] `eval_baseline` 仍有 32 个测试
- [x] `just runtime-lifecycle-check` 的 263 个测试在 nextest 下 0 failed
- [x] CI macOS job 里只有一步跑 proc 测试
- [x] `CLAUDE.md` 只有一个 `## 分级` 章节，无 `Tier N`、无 `test-real-*`
- [x] `just --list` 里没有 `test-real-*`，有 `test-live`
- [x] `just ci` 全绿（在 `yi-agent-rs/` 下；这是合并前唯一入口）

---

## 执行记录（2026-11-08）

全部 5 个 Task 已实施并合入 `main`（`f88d1977`）。合并后又在合并结果上复验一轮。

### 提交

| 提交 | 内容 |
|---|---|
| `bb90d25b` | nextest：default 排除 live，新增 `[profile.live]` |
| `b0ae00d7` | `just test-live` 取代 4 个 `test-real-*` recipe |
| `332a04f4` | CI：macOS job 删除重复的 lifecycle 步骤 |
| `a6a6ad63` | CLAUDE.md：两个「分级」章节合并为双轴 |
| `01dbfeea` | 进度文档：记录保真度轴 |
| `14f62835` | 修 `nightly.yml` 仍调旧 recipe；provider 映射覆盖第三方 |

### 实测值

合并后（`f88d1977` + 后续 runtime-settings 合并）在真空闲环境（load 4.25）测得：

- 全量：**6250 passed / 0 failed / 41 skipped**
- `just ci` 22 项门禁全部通过
- `--profile live --run-ignored only`：**18**（6+5+4+2+1）
- workspace 默认 profile 含 live：**0**
- `--run-ignored all`（加 filter 后）：**15**（加 filter 前为 33）
- `--profile ci` 继承 default：**6250**
- `eval_baseline`：**32**
- `just --list`：`test-real-*` 为 0 个，`test-live` 为 1 个
- CI macOS job 跑测试的步骤：**1 个**

测试总数从 6020 涨到 6250，是因为同期另一个合并（`ad100876` runtime settings）新增了 230 个测试，与本计划无关。

### 实施中额外发现并修掉的缺陷（计划未预料）

1. **`nightly.yml` 调用了已删除的 `test-real-all`** —— 被 `nightly-workflow-check`
   抓到（`13 passed, 1 failed`）。同类的失效引用还在三份活跃文档里（
   `subagent-runtime.md`、`ci-cd.md`、`side-conversation-acceptance.md`），一并改掉。
   这正是该门禁存在的理由：「改了个 recipe 名，workflow 还在调旧名」靠静读发现不了。
2. **provider 映射会假红** —— 初版只把 `anthropic`/`openai` 映射到
   `ANTHROPIC_API_KEY`/`OPENAI_API_KEY`。第三方 provider（deepseek、moonshot、
   openrouter、本地 vLLM）会拿到 `MODEL_API_KEY` 却拿不到任何专属变量，测试报
   「缺 key」失败，看起来像真实回归。改为「非 anthropic 一律映射到
   `OPENAI_API_KEY`」——除 Anthropic 原生协议外都是 OpenAI 兼容，枚举第三方名字
   数量无界。
3. **一个 suite 失败会吞掉其余 suite** —— 初版循环里 `set -e` 让第一个失败的
   suite 直接中止，后面 4 个永不执行。改为收集失败、全部跑完后统一非零退出。

### 关于验证环境的注意事项

这台机器常有多会话并行编译（`session-completeness-impl`、
`fix-managed-session-container-attach`、`runtime-settings-live-apply` 等 worktree）。
load average 冲到 40–76 时，proc 类测试会因调度延迟超时失败，表现为随机几个
测试一起红（如 `refresh::tests::*` 多个同时 15s 超时）。实测同一批测试在 load 4.25
时 6250 全绿。判断依据：失败测试单独跑能否稳定通过 + 当时 `uptime` 的 load。

`quitting_one_frontend_leaves_the_others_work_running` 在 `just ci` 里偶发失败过一次；
单独跑 3 次均 16/16 通过，且本计划未触碰该文件。属既有负载敏感问题。
