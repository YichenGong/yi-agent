# 权限管理遗漏补齐实现计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 补齐权限管理功能的遗漏:blocklist 正则漏洞、黑名单不可持久化、代码重复、非原子写入、编译告警、LLM 前缀提取未接入、TUI 测试缺失。

**Architecture:** 7 个独立任务,每个可独立提交。优先级:安全修复 > 行为守卫 > 代码质量 > 测试。TDD 风格,每任务独立提交。

**Tech Stack:** Rust,regex,rstest,tokio,tempfile

---

## 遗漏清单

| # | 遗漏 | 严重性 | 任务 |
|---|------|--------|------|
| 1 | `rm -fr /`、`rm -r -f /`、`rm -rf --no-preserve-root /` 未拦截 | 安全 | Task 1 |
| 2 | 裸 `reboot`/`halt`/`poweroff` 未拦截(正则要求 `\s+`) | 安全 | Task 1 |
| 3 | 带空格的 fork bomb `: () { : | & } ; :` 未拦截 | 安全 | Task 1 |
| 4 | 黑名单命令可被 `AlwaysAllowTool` 持久化到白名单 | 行为 | Task 2 |
| 5 | agent.rs 中 NeedConfirm/Blacklisted ~110 行重复 | 质量 | Task 3 |
| 6 | `save_config` 非原子写入(崩溃可能损坏配置) | 质量 | Task 4 |
| 7 | `apply_decision` 的 `tool_input` 参数未使用(告警) | 质量 | Task 5 |
| 8 | `HistoryCell::PermissionResolved.request_id` 字段未读(告警) | 质量 | Task 5 |
| 9 | `LlmPrefixExtractor` 未实现,仅 `fallback_prefix` 兜底 | 功能 | Task 6 |
| 10 | TUI 权限流程无测试(按键→决策→agent 继续) | 测试 | Task 7 |
| 11 | 无 "key '3' when no prefix" 测试 | 测试 | Task 7 |
| 12 | 无 "黑名单 + 各决策" 测试 | 测试 | Task 7 |

**不在范围内**(已知限制,保持现状):
- `echo "rm -rf /"` 误报拦截 — 修复需 shell 解析,超出范围;当前测试明确接受此行为(保守)
- `rm -rf *`、`rm -rf ./` 未拦截 — 设计上只拦 `/`、`~/`、`$HOME`,符合预期

## 关键代码位置

- **blocklist 正则**:`yi-agent-rs/crates/yi-agent-tools/src/shell/blocklist.rs:9-70`
- **blocklist 测试**:`同文件:82-272`
- **apply_decision**:`yi-agent-rs/crates/yi-agent-core/src/permission.rs:218-260`
- **save_config**:`yi-agent-rs/crates/yi-agent-core/src/permission.rs:261-268`
- **agent ACT 循环**:`yi-agent-rs/crates/yi-agent-core/src/agent.rs:340-478`
- **TUI handle_key**:`yi-agent-rs/crates/yi-agent/src/tui/app.rs:183-262`
- **HistoryCell**:`yi-agent-rs/crates/yi-agent/src/tui/cell.rs`

## 实现顺序

- Task 1: blocklist 正则修复(安全)
- Task 2: 黑名单不可持久化(行为守卫)
- Task 3: 提取共享 helper(质量,减少重复)
- Task 4: 原子写入(质量)
- Task 5: 修编译告警(质量)
- Task 6: LlmPrefixExtractor(功能)
- Task 7: TUI 测试(测试)

---

### Task 1: 修复 blocklist 正则漏洞

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/shell/blocklist.rs`

**背景**:当前正则有多个漏洞导致危险命令漏拦。测试用例已记录这些为 `false`(未拦),需改正则 + 改测试期望为 `true`。

**Step 1: 修改正则**

编辑 `yi-agent-rs/crates/yi-agent-tools/src/shell/blocklist.rs`,在 `PATTERNS.get_or_init` 里:

**1a. rm -rf 变体** — 替换第 9 行的 `rm\s+-rf?\s+/\s*(--)?` 为更宽的模式:

```rust
            // 拦截 rm -rf / 及变体: -rf, -fr, -r -f, -f -r, --no-preserve-root
            (
                Regex::new(r"rm\s+(-[rfRF]+\s+|--no-preserve-root\s+|-r\s+-f\s+|-f\s+-r\s+).*\s+/(\s|$|--)")
                    .unwrap(),
                "rm -rf /",
            ),
```

注意:`-[rfRF]+` 匹配 `-rf`、`-fr`、`-Rf` 等任意 r/f 组合;`-r\s+-f` 和 `-f\s+-r` 匹配分离写法;`.*\s+/` 允许 `--no-preserve-root` 等参数在中间。

**1b. reboot/halt/poweroff 裸命令** — 替换第 39-41 行:

```rust
            (Regex::new(r"reboot(\s|$)").unwrap(), "reboot"),
            (Regex::new(r"halt(\s|$)").unwrap(), "halt"),
            (Regex::new(r"poweroff(\s|$)").unwrap(), "poweroff"),
```

`(\s|$)` 匹配尾随空格或字符串结尾,裸命令也会被拦。

**1c. 带空格的 fork bomb** — 替换第 12 行为更宽松的模式:

```rust
            (
                Regex::new(r":\s*\(\s*\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;\s*:").unwrap(),
                "fork bomb",
            ),
```

允许 `: () { : | : & } ; :` 这类带空格的变体。

**Step 2: 更新测试期望值**

在 `mod tests` 里,把以下测试用例的 `false` 改为 `true`(因为正则修复后它们应该被拦):

```rust
    #[case::rm_fr_root("rm -fr /", true)]              // 原 false → true
    #[case::rm_r_f_root("rm -r -f /", true)]            // 原 false → true
    #[case::rm_rf_no_preserve("rm -rf --no-preserve-root /", true)]  // 原 false → true
```

以及系统控制命令:

```rust
    #[case::reboot("reboot", true)]    // 原 false → true
    #[case::halt("halt", true)]        // 原 false → true
    #[case::poweroff("poweroff", true)]  // 原 false → true
```

以及 fork bomb 带空格:

```rust
    #[case::with_spaces(": () { : | & } ; :", true)]  // 原 false → true
```

**Step 3: 验证测试**

```bash
cargo test -p yi-agent-tools --manifest-path yi-agent-rs/Cargo.toml shell::blocklist::tests
```

Expected: 所有测试通过(改了正则后,原本 `false` 的用例现在 `true`,匹配新正则)

**重要**:如果有测试失败,说明正则修改后多拦或少拦了什么,调整正则或测试直到全部通过。

**Step 4: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-tools/src/shell/blocklist.rs
git commit -m "fix(blocklist): cover rm -fr, separate flags, bare reboot/halt/poweroff, spaced fork bomb"
```

---

### Task 2: 黑名单命令不可持久化到白名单

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/permission.rs`

**背景**:设计决策 C "黑名单命令默认拒绝,可手动确认执行"意味着每次都要确认,不应进白名单。当前 `apply_decision` 对黑名单命令的 `AlwaysAllowTool`/`AlwaysAllowPrefix` 也持久化,违反设计。

**Step 1: 写失败测试**

在 `permission.rs` 的 `mod tests` 末尾加:

```rust
    #[tokio::test]
    async fn apply_decision_blacklisted_always_allow_tool_rejected() {
        let tmpdir = tempfile::tempdir().unwrap();
        let workdir = tmpdir.path().to_path_buf();
        let blocklist: BlocklistFn = Arc::new(|cmd: &str| {
            if cmd.contains("rm -rf /") {
                Some("rm -rf /".to_string())
            } else {
                None
            }
        });
        let checker = PermissionChecker::new(PermissionsConfig::default(), false, workdir.clone(), blocklist);

        // 模拟黑名单命令的用户决策:AlwaysAllowTool
        // apply_decision 不接受 kind 参数,所以需要在 check 流程里判断
        // 但 apply_decision 本身不知道是不是黑名单,所以这个守卫要在 agent.rs 里加
        // 这里测试 apply_decision 本身的行为:它仍然会持久化
        // 真正的守卫在 agent.rs 的 Blacklisted 分支
        checker
            .apply_decision("bash", &bash_input("rm -rf /"), &Decision::AlwaysAllowTool)
            .await
            .unwrap();

        // 验证:当前 apply_decision 没有黑名单感知,会持久化
        // 这个测试记录当前行为,Task 2 的修改在 agent.rs 里
        let loaded = PermissionChecker::load(&workdir).await.unwrap();
        assert!(loaded.tool_level.bash, "apply_decision persists without blacklist awareness");
    }
```

实际上,更好的做法是给 `apply_decision` 加一个 `is_blacklisted: bool` 参数,或者在 agent.rs 的 Blacklisted 分支不调 `apply_decision`。

**方案**:在 `apply_decision` 加 `kind: &PermissionKind` 参数,当 `kind` 是 `Blacklisted` 时,拒绝 `AlwaysAllowTool` 和 `AlwaysAllowPrefix`(只允许 `AllowOnce` 和 `Deny`)。

修改 `apply_decision` 签名:

```rust
    pub async fn apply_decision(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        decision: &Decision,
        kind: &PermissionKind,
    ) -> Result<(), String> {
        // 黑名单命令不可持久化
        if matches!(kind, PermissionKind::Blacklisted(_)) {
            if matches!(decision, Decision::AlwaysAllowTool | Decision::AlwaysAllowPrefix(_)) {
                return Err("blacklisted commands cannot be added to whitelist");
            }
        }
        // ... 原有逻辑 ...
    }
```

**Step 2: 修改测试**

更新上面的测试:

```rust
    #[tokio::test]
    async fn apply_decision_blacklisted_always_allow_tool_rejected() {
        let tmpdir = tempfile::tempdir().unwrap();
        let workdir = tmpdir.path().to_path_buf();
        let blocklist: BlocklistFn = Arc::new(|_| None);
        let checker = PermissionChecker::new(PermissionsConfig::default(), false, workdir.clone(), blocklist);

        let kind = PermissionKind::Blacklisted("rm -rf /".to_string());
        let result = checker
            .apply_decision("bash", &bash_input("rm -rf /"), &Decision::AlwaysAllowTool, &kind)
            .await;
        assert!(result.is_err(), "AlwaysAllowTool on blacklisted should be rejected");
        let loaded = PermissionChecker::load(&workdir).await.unwrap();
        assert!(!loaded.tool_level.bash, "should not persist");
    }

    #[tokio::test]
    async fn apply_decision_blacklisted_allow_once_allowed() {
        let tmpdir = tempfile::tempdir().unwrap();
        let workdir = tmpdir.path().to_path_buf();
        let blocklist: BlocklistFn = Arc::new(|_| None);
        let checker = PermissionChecker::new(PermissionsConfig::default(), false, workdir.clone(), blocklist);

        let kind = PermissionKind::Blacklisted("rm -rf /".to_string());
        let result = checker
            .apply_decision("bash", &bash_input("rm -rf /"), &Decision::AllowOnce, &kind)
            .await;
        assert!(result.is_ok(), "AllowOnce on blacklisted should be allowed (not persisted)");
        let loaded = PermissionChecker::load(&workdir).await.unwrap();
        assert!(!loaded.tool_level.bash, "AllowOnce should not persist");
    }

    #[tokio::test]
    async fn apply_decision_blacklisted_deny_allowed() {
        let tmpdir = tempfile::tempdir().unwrap();
        let workdir = tmpdir.path().to_path_buf();
        let blocklist: BlocklistFn = Arc::new(|_| None);
        let checker = PermissionChecker::new(PermissionsConfig::default(), false, workdir.clone(), blocklist);

        let kind = PermissionKind::Blacklisted("rm -rf /".to_string());
        let result = checker
            .apply_decision("bash", &bash_input("rm -rf /"), &Decision::Deny, &kind)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn apply_decision_normal_always_allow_tool_still_works() {
        let tmpdir = tempfile::tempdir().unwrap();
        let workdir = tmpdir.path().to_path_buf();
        let blocklist: BlocklistFn = Arc::new(|_| None);
        let checker = PermissionChecker::new(PermissionsConfig::default(), false, workdir.clone(), blocklist);

        let kind = PermissionKind::Normal;
        checker
            .apply_decision("bash", &bash_input("ls"), &Decision::AlwaysAllowTool, &kind)
            .await
            .unwrap();
        let loaded = PermissionChecker::load(&workdir).await.unwrap();
        assert!(loaded.tool_level.bash, "Normal AlwaysAllowTool should persist");
    }
```

**Step 3: 更新现有 apply_decision 测试的调用**

现有所有 `apply_decision` 调用需要加 `&PermissionKind::Normal` 参数。找到所有调用点(在测试里),加 `&PermissionKind::Normal`。

同时在 `agent.rs` 里找到 `apply_decision` 的两个调用点(在 NeedConfirm 和 Blacklisted 分支),加 `&req.kind` 参数。

**Step 4: 运行测试**

```bash
cargo test -p yi-agent-core --manifest-path yi-agent-rs/Cargo.toml permission::tests
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-core agent::tests
```

Expected: 所有测试通过

**Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/permission.rs yi-agent-rs/crates/yi-agent-core/src/agent.rs
git commit -m "fix(permission): blacklisted commands cannot be persisted to whitelist"
```

---

### Task 3: 提取共享 handle_confirmation helper

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`

**背景**:`run_loop` 的 ACT 循环里,`NeedConfirm` 和 `Blacklisted` 分支有 ~110 行几乎相同的代码(发 PermissionRequest 事件、等决策、发 PermissionResolved、匹配决策、apply 或 deny)。

**Step 1: 提取 helper 函数**

在 `agent.rs` 的 `wait_for_decision` 函数附近,加一个 `handle_confirmation` 异步函数:

```rust
/// 处理需要用户确认的权限请求(NeedConfirm 或 Blacklisted)。
/// 返回 Some((id, input)) 如果用户允许执行,返回 None 如果用户拒绝。
async fn handle_confirmation(
    tx: &mpsc::Sender<AgentEvent>,
    checker: &Arc<crate::permission::PermissionChecker>,
    decision_rx: &Arc<tokio::sync::Mutex<mpsc::Receiver<(u64, crate::permission::Decision)>>>,
    cancel_token: &tokio_util::sync::CancellationToken,
    id: String,
    name: String,
    input: Value,
    req: crate::permission::PermissionRequest,
    denied_message: &str,
) -> Option<(String, String, Value)> {
    let _ = tx.send(AgentEvent::PermissionRequest {
        request_id: req.request_id,
        tool_name: req.tool_name.clone(),
        tool_input: req.tool_input.clone(),
        prefix_suggestion: req.prefix_suggestion.clone(),
        kind: req.kind.clone(),
    }).await;

    let decision = wait_for_decision(decision_rx, req.request_id, cancel_token).await;

    let _ = tx.send(AgentEvent::PermissionResolved {
        request_id: req.request_id,
        decision: decision.clone(),
    }).await;

    match decision {
        crate::permission::Decision::AllowOnce
        | crate::permission::Decision::AlwaysAllowTool
        | crate::permission::Decision::AlwaysAllowPrefix(_) => {
            if let Err(e) = checker.apply_decision(&name, &input, &decision, &req.kind).await {
                tracing::warn!("failed to persist permission decision: {e}");
            }
            Some((id, name, input))
        }
        crate::permission::Decision::Deny => {
            let _ = tx.send(AgentEvent::ToolResult {
                id: id.clone(),
                result: ToolResult::error(denied_message),
            }).await;
            None
        }
    }
}
```

**Step 2: 替换 NeedConfirm 和 Blacklisted 分支**

在 ACT 循环里,替换两个分支:

```rust
            crate::permission::CheckResult::NeedConfirm(req) => {
                if let Some(decision_rx) = &decision_rx {
                    if let Some((id, name, input)) = handle_confirmation(
                        &tx, &checker, decision_rx, &cancel_token,
                        id, name, input, req, "user denied",
                    ).await {
                        checked_uses.push((id, name, input));
                    } else {
                        denied_results.push((id, ToolResult::error("user denied")));
                    }
                } else {
                    let _ = tx.send(AgentEvent::ToolResult {
                        id: id.clone(),
                        result: ToolResult::error("permission required but no decision channel"),
                    }).await;
                    denied_results.push((id, ToolResult::error("permission required but no decision channel")));
                }
            }
            crate::permission::CheckResult::Blacklisted(req) => {
                if let Some(decision_rx) = &decision_rx {
                    if let Some((id, name, input)) = handle_confirmation(
                        &tx, &checker, decision_rx, &cancel_token,
                        id, name, input, req, "user denied blacklisted command",
                    ).await {
                        checked_uses.push((id, name, input));
                    } else {
                        denied_results.push((id, ToolResult::error("user denied blacklisted command")));
                    }
                } else {
                    let _ = tx.send(AgentEvent::ToolResult {
                        id: id.clone(),
                        result: ToolResult::error("blacklisted command requires confirmation"),
                    }).await;
                    denied_results.push((id, ToolResult::error("blacklisted command requires confirmation")));
                }
            }
```

注意:`denied_results` 的 push 需要配合 — 当 `handle_confirmation` 返回 `None` 时,把错误结果也加到 `denied_results`。

**Step 3: 运行测试**

```bash
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-core
```

Expected: 所有测试通过(行为不变,只是代码重构)

**Step 4: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs
git commit -m "refactor(agent): extract handle_confirmation helper to reduce duplication"
```

---

### Task 4: 原子写入 save_config

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/permission.rs`

**背景**:`save_config` 用 `std::fs::write` 直接写目标文件,进程崩溃可能损坏配置。用"写临时文件 + rename"模式保证原子性。

**Step 1: 修改 save_config**

```rust
    fn save_config(&self, config: &PermissionsConfig) -> std::io::Result<()> {
        let dir = self.workdir.join(".yi-agent");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("permissions.toml");
        let toml_str = toml::to_string(config)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // 原子写入:先写临时文件,再 rename
        let tmp_path = dir.join("permissions.toml.tmp");
        std::fs::write(&tmp_path, &toml_str)?;
        std::fs::rename(&tmp_path, &path)
    }
```

注意:rename 在同一文件系统内是原子的(POSIX 保证)。临时文件 `.tmp` 后缀在失败时残留,但 `save_config` 下次调用会覆盖它。

**Step 2: 加测试验证原子性(行为不变)**

现有 `apply_*` 测试已经验证写入功能,不需要额外测试。但加一个测试验证 `.tmp` 不残留:

```rust
    #[tokio::test]
    async fn save_config_does_not_leave_tmp_file() {
        let tmpdir = tempfile::tempdir().unwrap();
        let workdir = tmpdir.path().to_path_buf();
        let blocklist: BlocklistFn = Arc::new(|_| None);
        let checker = PermissionChecker::new(PermissionsConfig::default(), false, workdir.clone(), blocklist);

        checker
            .apply_decision("bash", &bash_input("ls"), &Decision::AlwaysAllowTool, &PermissionKind::Normal)
            .await
            .unwrap();

        let tmp = workdir.join(".yi-agent").join("permissions.toml.tmp");
        assert!(!tmp.exists(), "tmp file should not remain after atomic write");
        let final_path = workdir.join(".yi-agent").join("permissions.toml");
        assert!(final_path.exists());
    }
```

**Step 3: 运行测试**

```bash
cargo test -p yi-agent-core --manifest-path yi-agent-rs/Cargo.toml permission::tests
```

Expected: 所有测试通过

**Step 4: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/permission.rs
git commit -m "fix(permission): atomic config write via tmp file + rename"
```

---

### Task 5: 修编译告警

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/permission.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/cell.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`(apply_decision 调用处)

**Step 1: 移除 apply_decision 的未使用参数 `tool_input`**

在 `permission.rs` 的 `apply_decision` 方法,把 `tool_input` 参数改为 `_tool_input`(Task 2 已经改过签名加了 `kind`,这里只处理 `tool_input`):

```rust
    pub async fn apply_decision(
        &self,
        tool_name: &str,
        _tool_input: &serde_json::Value,  // 保留参数以备未来扩展,当前未用
        decision: &Decision,
        kind: &PermissionKind,
    ) -> Result<(), String> {
```

或者完全移除参数(更干净,但会改变签名,需要更新所有调用处)。推荐移除:

```rust
    pub async fn apply_decision(
        &self,
        tool_name: &str,
        decision: &Decision,
        kind: &PermissionKind,
    ) -> Result<(), String> {
```

然后更新所有调用处(在 `agent.rs` 的 `handle_confirmation` 里和测试里),移除 `tool_input` 参数。

**Step 2: 处理 HistoryCell::PermissionResolved 的未读 request_id**

在 `yi-agent-rs/crates/yi-agent/src/tui/cell.rs`,检查 `PermissionResolved` 变体。`request_id` 字段存储但从未读。

两个选择:
- **A. 移除字段**(干净,但改变了数据结构)
- **B. 用 `#[allow(dead_code)]` 标注**(保留字段以备未来)

推荐 **A**:移除 `request_id` 字段。如果未来需要,再加回。

```rust
    PermissionResolved {
        decision: yi_agent_core::permission::Decision,
    },
```

更新所有构造 `PermissionResolved` 的地方,移除 `request_id` 字段。

**Step 3: 验证编译无告警**

```bash
cargo build --manifest-path yi-agent-rs/Cargo.toml 2>&1 | grep "warning" | head -20
```

Expected:权限相关的告警全部消除(可能有其他预先存在的告警)

**Step 4: 运行测试**

```bash
cargo test --manifest-path yi-agent-rs/Cargo.toml
```

Expected: 所有测试通过

**Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/permission.rs yi-agent-rs/crates/yi-agent-core/src/agent.rs yi-agent-rs/crates/yi-agent/src/tui/cell.rs yi-agent-rs/crates/yi-agent/src/tui/history.rs
git commit -m "fix: remove unused tool_input param and PermissionResolved.request_id field"
```

---

### Task 6: 实现 LlmPrefixExtractor

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/llm_prefix.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/lib.rs`(若需要)

**背景**:设计规定 LLM 提取前缀(15 秒超时),失败降级到 `fallback_prefix`。目前只有 `fallback_prefix`,`LlmPrefixExtractor` 未实现。

**Step 1: 检查 Provider trait**

读 `yi-agent-rs/crates/yi-agent-core/src/provider.rs` 和 `yi-agent-rs/crates/yi-agent-llm/` 看 Provider trait 有没有非流式完成方法。如果没有,需要加一个,或用流式 API 收集第一个 chunk。

**Step 2: 创建 llm_prefix.rs**

```rust
use std::sync::Arc;
use std::time::Duration;
use async_trait::async_trait;
use yi_agent_core::permission::PrefixExtractor;
use yi_agent_core::Provider;  // 确认实际路径

pub struct LlmPrefixExtractor {
    provider: Arc<dyn Provider>,
    model: String,
}

impl LlmPrefixExtractor {
    pub fn new(provider: Arc<dyn Provider>, model: String) -> Self {
        Self { provider, model }
    }
}

#[async_trait]
impl PrefixExtractor for LlmPrefixExtractor {
    async fn extract(&self, command: &str) -> Option<String> {
        // 极短命令不调 LLM
        if command.split_whitespace().count() <= 1 {
            return Some(command.trim().to_string());
        }
        let prompt = format!(
            "从以下 shell 命令提取命令前缀(命令名 + 子命令,不含参数)。只返回前缀字符串,不要其他内容。\n命令: {command}"
        );
        let fut = self.provider.complete(&self.model, &prompt);
        match tokio::time::timeout(Duration::from_secs(15), fut).await {
            Ok(Ok(text)) => {
                let trimmed = text.trim();
                if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
            }
            _ => None,
        }
    }
}
```

注意:需确认 Provider trait 有没有 `complete` 方法。如果没有,需要先在 `yi-agent-core` 的 Provider trait 里加一个 `async fn complete(&self, model: &str, prompt: &str) -> Result<String, ProviderError>` 的默认实现(用流式 API 收集)。

**Step 3: 在 main.rs 接入**

在 `run_agent` 里,构造 `LlmPrefixExtractor` 并传给 `PermissionChecker`。但 `PermissionChecker` 在 `yi-agent-core`,不持有 `PrefixExtractor`。

**方案**:在 `build_request` 里,如果有 `PrefixExtractor` 就调它,否则用 `fallback_prefix`。给 `PermissionChecker` 加一个 `prefix_extractor: Option<Arc<dyn PrefixExtractor>>` 字段。

修改 `PermissionChecker`:

```rust
pub struct PermissionChecker {
    config: Mutex<PermissionsConfig>,
    yolo: bool,
    workdir: std::path::PathBuf,
    blocklist_fn: BlocklistFn,
    next_request_id: AtomicU64,
    prefix_extractor: Option<Arc<dyn PrefixExtractor>>,
}
```

`new` 加参数 `prefix_extractor: Option<Arc<dyn PrefixExtractor>>`。

`build_request` 改为:

```rust
    fn build_request(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        kind: PermissionKind,
    ) -> PermissionRequest {
        let prefix_suggestion = if tool_name == "bash" {
            let cmd = tool_input.get("command").and_then(|v| v.as_str());
            cmd.and_then(|c| {
                // 同步上下文不能调 async extract,所以 build_request 不能直接调
                // 方案:build_request 返回 None,在 agent loop 里单独调 extract
                // 或者把 build_request 改成 async
                fallback_prefix(c)
            })
        } else {
            None
        };
        // ...
    }
```

**问题**:`build_request` 是同步的,`PrefixExtractor::extract` 是 async。

**方案**:`check()` 改为 async,在需要确认时调 `extract`。或者 `check()` 保持同步返回 `CheckResult`,但 `CheckResult::NeedConfirm(req)` 里的 `req.prefix_suggestion` 为 `None`,然后在 agent loop 里(`build_request` 之后)异步调 `extract` 填充。

**最简方案**:保持 `check()` 同步,`build_request` 用 `fallback_prefix` 填充。在 `agent.rs` 的 `handle_confirmation` 里,如果是 bash 且有 `prefix_extractor`,异步调 `extract` 更新 `req.prefix_suggestion`,然后再发 `PermissionRequest` 事件。

但这样 `handle_confirmation` 需要访问 `prefix_extractor`。

**更简方案(推荐)**:不在 `PermissionChecker` 里持有 `prefix_extractor`。在 `agent.rs` 里,当 `check()` 返回 `NeedConfirm` 或 `Blacklisted` 时,如果是 bash,异步调 `prefix_extractor.extract(cmd)`(如果有),更新 `req.prefix_suggestion`,然后进 `handle_confirmation`。

但这需要 `agent.rs` 持有 `prefix_extractor`。给 `Agent` 加一个 `prefix_extractor: Option<Arc<dyn PrefixExtractor>>` 字段。

**最简单实现**:第一版 `LlmPrefixExtractor` 只做结构搭建 + 单元测试,不接入 agent loop(因为接入需要改 `Agent` 签名,且 `fallback_prefix` 已工作)。留一个 TODO 注释。

**简化 Task 6 范围**:
1. 在 `yi-agent-core` 的 Provider trait 加 `complete` 方法(如果有默认实现就用,否则加)
2. 在 `yi-agent` crate 实现 `LlmPrefixExtractor`
3. 单元测试用 mock provider
4. **不接入 agent loop**(留 TODO,后续迭代)

**Step 3: 实现 Provider::complete(若需要)**

读 `yi-agent-rs/crates/yi-agent-core/src/provider.rs`,检查有没有非流式方法。如果没有,加一个带默认实现的方法(用流式 API 收集):

```rust
async fn complete(&self, model: &str, prompt: &str) -> Result<String, ProviderError> {
    // 默认实现:用 stream 收集所有文本
    let stream = self.stream(model, prompt, /* params */).await?;
    let mut full_text = String::new();
    use futures::StreamExt;
    let mut stream = Box::pin(stream);
    while let Some(chunk) = stream.next().await {
        match chunk? {
            ProviderEvent::Text(t) => full_text.push_str(&t),
            _ => {}
        }
    }
    Ok(full_text)
}
```

(具体签名取决于现有 Provider trait)

**Step 4: 实现 LlmPrefixExtractor + 测试**

创建 `yi-agent-rs/crates/yi-agent/src/llm_prefix.rs`,实现 `LlmPrefixExtractor`。

测试用 mock provider:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // mock provider 返回固定文本
    // 测试 extract 成功、超时、空返回
}
```

**Step 5: 验证编译和测试**

```bash
cargo build --manifest-path yi-agent-rs/Cargo.toml
cargo test --manifest-path yi-agent-rs/Cargo.toml
```

**Step 6: 提交**

```bash
git add yi-agent-rs/crates/yi-agent/src/llm_prefix.rs yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent-core/src/provider.rs
git commit -m "feat(permission): LlmPrefixExtractor with 15s timeout (not yet wired to agent)"
```

---

### Task 7: TUI 权限流程测试

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`(在 `mod tests` 里加测试)

**背景**:TUI 的 `handle_key` 权限处理(1-4 + Enter)无测试覆盖。

**Step 1: 写测试 — 按键 1-4 处理**

在 `tui/app.rs` 的 `mod tests` 里,利用现有的 `ScriptedEvents` 和 `TestBackend` 基础设施加测试:

```rust
    #[test]
    fn permission_key_1_allows_once() {
        let (decision_tx, mut decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let mut history = HistoryState::new();
        // 推入一个 PermissionRequest
        history.push_event(AgentEvent::PermissionRequest {
            request_id: 1, tool_name: "bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            prefix_suggestion: Some("ls".into()),
            kind: yi_agent_core::permission::PermissionKind::Normal,
        }, 80);

        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)),
            // quit after
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]));
        let event_source = ScriptedEvents { events };

        // Run with a small terminal
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let is_running = Arc::new(AtomicBool::new(false));

        let _ = run_tui_with_backend_and_events(
            &mut terminal, &mut history, &mut InputLine::new(),
            &input_tx, &interrupt_tx, &is_running, &event_source, &decision_tx,
        );

        // Verify decision was sent
        let decision = decision_rx.blocking_recv();
        assert_eq!(decision, Some((1, yi_agent_core::permission::Decision::AllowOnce)));
    }
```

类似地加:
- `permission_key_2_always_allow_tool`
- `permission_key_3_always_allow_prefix`
- `permission_key_4_deny`
- `permission_enter_defaults_to_allow_for_normal`
- `permission_enter_defaults_to_deny_for_blacklisted`
- `permission_key_3_when_no_prefix_is_noop`
- `permission_other_keys_ignored_while_pending`

**Step 2: 运行测试**

```bash
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent tui::app::tests
```

Expected: 所有新测试通过

**Step 3: 提交**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/app.rs
git commit -m "test(tui): permission key handling flow tests"
```

---

## 实现后验证

完成所有任务后:

1. `cargo build --manifest-path yi-agent-rs/Cargo.toml` 无权限相关告警
2. `cargo test --manifest-path yi-agent-rs/Cargo.toml` 全部通过
3. 手动测试:
   - `yi-agent` 触发 bash,确认 4 选项出现
   - 选"3"时前缀正确提取和持久化
   - 黑名单命令(`rm -rf /`)选"2"后,下次仍需确认(不可持久化)
   - `yi-agent --yolo` 黑名单命令仍弹确认
   - `reboot`(裸命令)被拦截
