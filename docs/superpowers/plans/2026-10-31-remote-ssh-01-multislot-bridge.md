# 远程 SSH 执行 · 计划 1：多槽位桥接层与远端连接骨架

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把桌面桥接层的单例 sidecar 改造成按槽位分键的进程表，并在同一骨架上接入"每项目一条 ssh 长连到的远端 app-server"，从而让本机可以同时连 N 个远端项目。

**Architecture:** `Sidecar` 从 `Mutex<Option<CommandChild>>` 变成 `Mutex<HashMap<ProfileKey, Arc<Mutex<Option<CommandChild>>>>>`——外层锁只保护查表，每个槽位一把内层锁，避免某个远端管道写阻塞时拖死其它槽位。每个槽位一条独立监督循环，事件名带槽位后缀，远端槽位通过 ssh 子进程承载 `app-server --listen stdio://`（stdio 就是 ssh 的管道）。

**Tech Stack:** Rust（Tauri 2、tauri-plugin-shell）、TypeScript（React 18、vitest）、`yi-agent-flags` 自有开关系统、ssh（系统二进制，不引库）。

**依据 spec:** `docs/superpowers/specs/2026-10-31-remote-ssh-execution-design.md`（下称 spec）。本计划覆盖 spec §10 的第 1、2、4、5、6 步。第 3 步（安装）见计划 2，第 7–12 步见计划 3，第 13–14 步见计划 4。

## Global Constraints

- **禁止在 `main` 上直接提交。** 全部工作在 `git worktree add .worktrees/<name> -b <type>/<name>` 出来的分支上做，完成后 `git merge --no-ff` 回 main。
- **提交前必须 `cd yi-agent-rs && cargo fmt --all`**，否则 `just fmt-check` 会挂。
- **commit message 用 conventional commits，首行 ≤72 字符，不写 `Co-Authored-By: Claude`。**
- **flag 名固定为 `remote_ssh`**，`default: false`，`expires_on` 写 `2027-06-30`。所有 flag 默认值必须是 `false`（`every_flag_defaults_to_off` 测试强制）。
- **stable（`build_kind != "dev"`）构建下 `remote_ssh` 恒关**，且 `flags/*` 方法在 stable 上返回 `-32601`（见 `flags_rpc.rs:36`）。
- **`ProfileKey` 的字符串形式必须匹配 `^[a-z0-9][a-z0-9-]{0,31}$`**（spec §4.9）。它是事件名后缀、目录名末段，字符集不合法会污染两者。
- **事件名格式固定为 `app-server://{kind}/{slot}`**，`kind ∈ {message, request, status}`，`slot` 是 `ProfileKey` 的字符串形式。本地槽位固定为 `local`。
- **`YI_AGENT_HOME` 必须显式传给远端进程**，不能依赖 `cd`（spec §4.6 已核实：`instance_home()` 只看 env 与 `$HOME`，不看 cwd）。
- **远端路径必须用单引号包裹并做 shell 转义**，不允许直接拼接未校验的用户输入（spec §6 第 3 条）。
- **单测一律不依赖真实 ssh 或真实网络**：起进程的部分抽成纯函数（`remote_argv`），有 IO 的部分用临时目录与假二进制。

---

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-flags/src/registry.rs` | 注册 `remote_ssh` | 改 |
| `desktop/src-tauri/src/remote/mod.rs` | `remote` 模块入口，re-export | 建 |
| `desktop/src-tauri/src/remote/profile.rs` | `ProfileKey`、`RemoteProfile`、`SlotKey` 的类型与校验（纯逻辑） | 建 |
| `desktop/src-tauri/src/remote/argv.rs` | `remote_argv()`：把 profile 变成 ssh 命令行（纯函数，唯一拼 shell 的地方） | 建 |
| `desktop/src-tauri/src/remote/config.rs` | `remote.json` 读写（flock + 唯一临时名） | 建 |
| `desktop/src-tauri/src/bridge.rs` | 单例 → 槽位表；事件名带槽位；每槽位监督循环 | 改 |
| `desktop/src-tauri/src/lib.rs` | 注册新模块与新命令 | 改 |
| `desktop/src/tauriTransport.ts` | 事件名参数化 + `invoke` 带 slot | 改 |
| `desktop/src/lib/slots.ts` | 前端槽位常量与事件名拼装（与 Rust 侧对齐的唯一定义处） | 建 |

**为什么 `profile.rs` 和 `argv.rs` 分开**：前者是数据类型与校验，后者是"把数据变成 shell 命令"。spec §8.6 说这是唯一拼 shell 的地方，单独成文件才能让它只有纯函数、可以被穷举测试。

**为什么 `config.rs` 单独成文件**：它有跨进程锁和原子写，是"有 IO 且难测"的一类，与纯逻辑分开才能各自用合适的测试手段。

---

### Task 1: 注册 `remote_ssh` flag

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-flags/src/registry.rs:37-49`（`REGISTRY` 数组）
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: 无（这是第一个任务）
- Produces: 常量 `REMOTE_SSH_FLAG: &str = "remote_ssh"`，`REGISTRY` 中多一项。后续任务用 `yi_agent_flags::is_enabled("remote_ssh")` 判定。

- [ ] **Step 1: 写失败测试**

在 `registry.rs` 的 `mod tests` 里，紧接着 `the_lead_agent_flag_is_registered` 之后加：

```rust
    #[test]
    fn the_remote_ssh_flag_is_registered() {
        let def = find(crate::registry::REMOTE_SSH_FLAG).expect("remote_ssh must be registered");
        assert!(!def.default, "remote ssh must default to off: it is dev-only");
        assert!(!def.summary.is_empty());
        assert!(!def.owner.is_empty());
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-flags --lib the_remote_ssh_flag_is_registered
```

Expected: 编译失败，`no REMOTE_SSH_FLAG in registry`。

- [ ] **Step 3: 实现**

在 `registry.rs` 的 `SUPERPOWERS_KANBAN_FLAG` 定义之后加常量：

```rust
/// 远程 SSH 执行的总开关。dev-only：stable 构建下恒关（spec §5.3.1）。
pub const REMOTE_SSH_FLAG: &str = "remote_ssh";
```

在 `REGISTRY` 数组末尾追加一项（注意 `REGISTRY` 顺序不影响功能，但保持与常量声明同序便于阅读）：

```rust
    FlagDef {
        name: REMOTE_SSH_FLAG,
        summary: "远程执行：经 ssh 在远端项目里跑常驻 app-server，手机可见",
        owner: "yichen",
        expires_on: "2027-06-30",
        default: false,
    },
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-flags --lib
```

Expected: 全绿，包括既有的 `every_flag_defaults_to_off` 与 `no_flag_is_past_its_expiry_date`。

- [ ] **Step 5: 验证 stable 闸门仍然有效（手工）**

```bash
cd yi-agent-rs && cargo test -p yi-agent-flags --lib a_release_build_must_not_honour
```

Expected: PASS（既有测试，确认新 flag 没有绕过 release 闸门）。

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-flags/src/registry.rs
git commit -m "feat(flags): register remote_ssh, default off"
```

---

### Task 2: `ProfileKey` 与校验（纯逻辑）

**Files:**
- Create: `desktop/src-tauri/src/remote/mod.rs`
- Create: `desktop/src-tauri/src/remote/profile.rs`
- Modify: `desktop/src-tauri/src/lib.rs`（加 `mod remote;`）
- Test: `desktop/src-tauri/src/remote/profile.rs` 内嵌 `mod tests`

**Interfaces:**
- Consumes: 无
- Produces:
  - `pub enum ProfileKey { Local, Remote(String) }`，`impl ProfileKey { pub fn as_str(&self) -> &str }`
  - `pub fn validate_profile_id(id: &str) -> Result<(), ProfileError>`
  - `pub struct RemoteProfile { pub id: String, pub label: String, pub host: String, pub local_path: PathBuf, pub remote_path: String, pub instance_root: String, pub binary_version: String, pub remote_arch: String }`
  - `pub enum ProfileError { BadId(String), BadHost(String), BadRemotePath(String), NotAbsolute(String) }`，`impl Display`

- [ ] **Step 1: 写失败测试**

`desktop/src-tauri/src/remote/mod.rs`：

```rust
//! 远程 SSH 执行：配置、命令行拼装、安装。
//!
//! 依据 `docs/superpowers/specs/2026-10-31-remote-ssh-execution-design.md`。

pub mod argv;
pub mod config;
pub mod profile;
```

（`argv` / `config` 在后续任务创建；本步先只声明 `profile`，避免编译失败。）

实际上本步 `mod.rs` 只写：

```rust
//! 远程 SSH 执行：配置、命令行拼装、安装。

pub mod profile;
```

`desktop/src-tauri/src/remote/profile.rs`：

```rust
//! `ProfileKey` 与 profile 字段校验。
//!
//! 字符集约束来自 spec §4.9：`id` 会被用作事件名后缀与目录名末段，
//! `host` / `remote_path` 会被拼进 ssh 命令行。三者都必须是白名单校验，
//! 而不是"看起来没问题就放过去"。

use std::fmt;
use std::path::PathBuf;

/// 一个进程槽的键。本地是唯一的内建槽位，远端的名字来自 `remote.json`。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProfileKey {
    Local,
    Remote(String),
}

impl ProfileKey {
    /// 事件名后缀与目录名末段都用它。本地固定 `local`。
    pub fn as_str(&self) -> &str {
        match self {
            ProfileKey::Local => "local",
            ProfileKey::Remote(id) => id,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProfileError {
    BadId(String),
    BadHost(String),
    BadRemotePath(String),
    NotAbsolute(String),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProfileError::BadId(v) => write!(
                f,
                "profile id `{v}` must match ^[a-z0-9][a-z0-9-]{{0,31}}$"
            ),
            ProfileError::BadHost(v) => write!(
                f,
                "host `{v}` must look like `user@host` with no shell metacharacters"
            ),
            ProfileError::BadRemotePath(v) => write!(
                f,
                "remote path `{v}` must be absolute and free of shell metacharacters"
            ),
            ProfileError::NotAbsolute(v) => write!(f, "path `{v}` must be absolute"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteProfile {
    pub id: String,
    pub label: String,
    pub host: String,
    pub local_path: PathBuf,
    pub remote_path: String,
    pub instance_root: String,
    pub binary_version: String,
    pub remote_arch: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_key_local_is_the_literal_local() {
        assert_eq!(ProfileKey::Local.as_str(), "local");
    }

    #[test]
    fn profile_key_remote_is_the_id() {
        assert_eq!(ProfileKey::Remote("proj-foo".into()).as_str(), "proj-foo");
    }

    #[test]
    fn profile_id_accepts_lowercase_digits_and_dash() {
        for ok in ["a", "proj-foo", "a1-b2", "z9"] {
            assert!(validate_profile_id(ok).is_ok(), "`{ok}` should be accepted");
        }
    }

    #[test]
    fn profile_id_rejects_uppercase_and_underscore_and_empty() {
        for bad in ["", "Proj", "proj_foo", "-proj", "proj foo", "proj.foo"] {
            assert!(validate_profile_id(bad).is_err(), "`{bad}` should be rejected");
        }
    }

    #[test]
    fn profile_id_rejects_more_than_32_chars() {
        let long = "a".repeat(33);
        assert!(validate_profile_id(&long).is_err());
        let exactly = "a".repeat(32);
        assert!(validate_profile_id(&exactly).is_ok());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test --lib remote::profile::tests
```

Expected: 编译失败，`cannot find function validate_profile_id`。

- [ ] **Step 3: 实现 `validate_profile_id`**

在 `profile.rs` 的 `RemoteProfile` 之后加：

```rust
/// `^[a-z0-9][a-z0-9-]{0,31}$`。
///
/// 不用正则库：这是全项目唯一一处需要匹配的字符集，手写一遍比引入依赖便宜，
/// 且失败信息能直接指出是哪个字符不合法。
pub fn validate_profile_id(id: &str) -> Result<(), ProfileError> {
    if id.is_empty() || id.len() > 32 {
        return Err(ProfileError::BadId(id.to_string()));
    }
    for (index, ch) in id.chars().enumerate() {
        let ok = if index == 0 {
            ch.is_ascii_lowercase() || ch.is_ascii_digit()
        } else {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'
        };
        if !ok {
            return Err(ProfileError::BadId(id.to_string()));
        }
    }
    Ok(())
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop/src-tauri && cargo test --lib remote::profile::tests
```

Expected: 5 passed。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/remote/mod.rs desktop/src-tauri/src/remote/profile.rs desktop/src-tauri/src/lib.rs
git commit -m "feat(remote): add ProfileKey and profile id validation"
```

---

### Task 3: `remote_argv` —— 唯一拼 shell 的地方

**Files:**
- Create: `desktop/src-tauri/src/remote/argv.rs`
- Modify: `desktop/src-tauri/src/remote/mod.rs`（加 `pub mod argv;`）
- Test: `desktop/src-tauri/src/remote/argv.rs` 内嵌 `mod tests`

**Interfaces:**
- Consumes: `RemoteProfile`（Task 2）
- Produces: `pub fn remote_argv(profile: &RemoteProfile, binary: &str, relay: Option<&str>) -> Result<Vec<String>, ProfileError>`。返回的是**传给 `ssh` 的参数**（不含 `ssh` 本身），形如 `["-o", "BatchMode=yes", "<host>", "cd '<path>' && exec env YI_AGENT_HOME='<root>' '<bin>' app-server --listen stdio://"]`。

**关键约束（spec §4.1 第 8 步、§4.6、§6 第 3 条）：**
- 远端命令是**一个** argv 元素（ssh 把剩余参数拼成远端命令）
- `cd` 决定 cwd，`env YI_AGENT_HOME` 决定状态目录，两者不可互相替代
- 用 `exec` 让 app-server 替换 shell，断线信号直达
- 路径一律单引号包裹，内部单引号按 `'\''` 转义
- `--relay` 必须出现在 `app-server --listen stdio://` 之后

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::profile::RemoteProfile;
    use std::path::PathBuf;

    fn sample() -> RemoteProfile {
        RemoteProfile {
            id: "proj-foo".into(),
            label: "foo".into(),
            host: "dev@10.0.0.5".into(),
            local_path: PathBuf::from("/Users/y/proj/foo"),
            remote_path: "/home/dev/proj/foo".into(),
            instance_root: "/home/dev/.yi-agent-projects/proj-foo/.yi-agent".into(),
            binary_version: "0.1.10".into(),
            remote_arch: "x86_64-unknown-linux-gnu".into(),
        }
    }

    #[test]
    fn argv_starts_with_host_then_one_remote_command() {
        let argv = remote_argv(&sample(), "/home/dev/.yi-agent/bin/0.1.10/yi-agent", None).unwrap();
        assert!(argv.len() >= 3, "ssh needs options, a host, and one remote command: {argv:?}");
        // 最后一个元素就是远端命令整串。
        let last = argv.last().unwrap();
        assert!(last.contains("app-server"), "remote command: {last}");
    }

    #[test]
    fn argv_sets_home_env_and_cd_separately() {
        let argv = remote_argv(&sample(), "/b/yi-agent", None).unwrap();
        let cmd = argv.last().unwrap();
        assert!(cmd.contains("cd '/home/dev/proj/foo'"), "cwd via cd: {cmd}");
        assert!(
            cmd.contains("YI_AGENT_HOME='/home/dev/.yi-agent-projects/proj-foo/.yi-agent'"),
            "state dir via env, NOT via cwd: {cmd}"
        );
        assert!(cmd.contains("exec "), "exec so signals reach app-server: {cmd}");
    }

    #[test]
    fn argv_sets_batch_mode_to_fail_fast() {
        let argv = remote_argv(&sample(), "/b/yi-agent", None).unwrap();
        assert!(
            argv.windows(2).any(|w| w[0] == "-o" && w[1] == "BatchMode=yes"),
            "a non-interactive ssh must not hang waiting for a password prompt: {argv:?}"
        );
    }

    #[test]
    fn argv_appends_relay_after_listen() {
        let argv = remote_argv(&sample(), "/b/yi-agent", Some("wss://r/connect?session=x")).unwrap();
        let cmd = argv.last().unwrap();
        let listen = cmd.find("--listen stdio://").expect("listen present");
        let relay = cmd.find("--relay").expect("relay present");
        assert!(relay > listen, "relay must come after listen: {cmd}");
        assert!(cmd.contains("'wss://r/connect?session=x'"), "relay quoted: {cmd}");
    }

    #[test]
    fn argv_omits_relay_when_none() {
        let argv = remote_argv(&sample(), "/b/yi-agent", None).unwrap();
        assert!(!argv.last().unwrap().contains("--relay"));
    }

    #[test]
    fn single_quotes_in_paths_are_escaped() {
        let mut p = sample();
        p.remote_path = "/home/dev/it's here".into();
        let argv = remote_argv(&p, "/b/yi-agent", None).unwrap();
        let cmd = argv.last().unwrap();
        // 闭合 → 拼接转义的单引号 → 重新打开
        assert!(cmd.contains(r"'/home/dev/it'\''s here'"), "escaped: {cmd}");
        assert!(!cmd.contains("it's here'"), "must not leak a bare quote: {cmd}");
    }

    /// spec §9 判据 4 按这个名字找测试，别改名。
    #[test]
    fn a_host_with_a_semicolon_is_rejected() {
        let mut p = sample();
        p.host = "dev@10.0.0.5;id".into();
        assert!(
            remote_argv(&p, "/b/yi-agent", None).is_err(),
            "a semicolon would break out of the ssh argument"
        );
    }

    #[test]
    fn shell_metacharacters_in_remote_path_are_rejected() {
        for bad in ["/home/dev/a;rm -rf /", "/home/dev/$(id)", "/home/dev/a`id`", "/home/dev/a|b"] {
            let mut p = sample();
            p.remote_path = bad.into();
            assert!(remote_argv(&p, "/b/yi-agent", None).is_err(), "`{bad}` must be rejected");
        }
    }

    #[test]
    fn relative_remote_path_is_rejected() {
        let mut p = sample();
        p.remote_path = "proj/foo".into();
        assert!(remote_argv(&p, "/b/yi-agent", None).is_err());
    }

    #[test]
    fn bad_host_is_rejected() {
        for bad in ["dev@10.0.0.5; rm -rf /", "dev host", "-oProxyCommand=evil"] {
            let mut p = sample();
            p.host = bad.into();
            assert!(remote_argv(&p, "/b/yi-agent", None).is_err(), "`{bad}` must be rejected");
        }
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test --lib remote::argv
```

Expected: 编译失败，`cannot find function remote_argv`。

- [ ] **Step 3: 实现**

```rust
//! 把 profile 变成一条 ssh 命令行。
//!
//! **这是全项目唯一拼 shell 的地方**（spec §8.6）。因此本文件必须保持纯函数：
//! 不读环境、不读文件、不起进程，输入的每个字段都已校验过，输出可以直接断言。
//!
//! 两件容易混淆的事，在下面的 `format!` 里是分开的两段：
//! - `cd '<remote_path>'` 决定**远端 app-server 的 cwd**（项目目录）
//! - `env YI_AGENT_HOME='<instance_root>'` 决定**状态目录**
//!
//! 二者不可互相替代：`instance_home()`（`store/src/paths.rs:20-23`）只看
//! `YI_AGENT_HOME` 与 `$HOME`，**完全不看 cwd**。只 `cd` 不设 env 会让所有远端
//! 项目共用一个状态目录，静默互相覆盖（spec §8.5）。

use super::profile::{ProfileError, RemoteProfile};

/// shell 单引号转义：`it's` → `it'\''s`，再由调用方整体包一层单引号。
fn sh_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', r"'\''"))
}

/// 远端路径的字符白名单。单引号已由 `sh_quote` 处理，这里拒绝的是
/// "即便转义了也不该出现"的东西——分号、反引号、`$()`、管道、换行。
fn path_is_safe(raw: &str) -> bool {
    if !raw.starts_with('/') {
        return false;
    }
    !raw.chars().any(|c| {
        matches!(
            c,
            ';' | '|' | '&' | '$' | '`' | '(' | ')' | '<' | '>' | '\n' | '\r' | '\\'
        )
    })
}

/// `user@host` 或 `host`，字符集与 spec §4.9 一致。
fn host_is_safe(raw: &str) -> bool {
    if raw.is_empty() || raw.starts_with('-') {
        // 前导 `-` 会被 ssh 当成选项（`-oProxyCommand=...`）
        return false;
    }
    raw.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
}

/// 拼出**传给 `ssh` 的参数**（不含 `ssh` 本身）。
///
/// 最后一个是完整的远端命令串：ssh 会把 host 之后的所有 argv 用空格拼起来交给
/// 远端 shell，所以这里刻意只放**一个**元素，避免参数被重复分词。
pub fn remote_argv(
    profile: &RemoteProfile,
    binary: &str,
    relay: Option<&str>,
) -> Result<Vec<String>, ProfileError> {
    if !host_is_safe(&profile.host) {
        return Err(ProfileError::BadHost(profile.host.clone()));
    }
    if !path_is_safe(&profile.remote_path) {
        return Err(ProfileError::BadRemotePath(profile.remote_path.clone()));
    }
    if !path_is_safe(&profile.instance_root) {
        return Err(ProfileError::BadRemotePath(profile.instance_root.clone()));
    }
    if !binary.starts_with('/') || !path_is_safe(binary) {
        return Err(ProfileError::NotAbsolute(binary.to_string()));
    }

    let mut command = format!(
        "cd {} && exec env YI_AGENT_HOME={} {} app-server --listen stdio://",
        sh_quote(&profile.remote_path),
        sh_quote(&profile.instance_root),
        sh_quote(binary),
    );
    if let Some(url) = relay {
        command.push_str(&format!(" --relay {}", sh_quote(url)));
    }

    Ok(vec![
        // 非交互：不要弹密码提示，失败就退出（否则 Tauri 侧会挂在一个永远等不到
        // 输入的 ssh 上，表现为"连接中"永不结束）。
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        // 连接超时，避免不可达主机把监督循环占住。
        "-o".to_string(),
        "ConnectTimeout=20".to_string(),
        profile.host.clone(),
        command,
    ])
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop/src-tauri && cargo test --lib remote::argv
```

Expected: 9 passed。

- [ ] **Step 5: 手工核对拼出来的命令**

```bash
cd desktop/src-tauri && cargo test --lib remote::argv -- --nocapture
```

若想肉眼看一眼真实产物，临时加一句 `println!("{argv:?}")` 到第一个测试里，确认形如：

```
["-o", "BatchMode=yes", "-o", "ConnectTimeout=20", "dev@10.0.0.5",
 "cd '/home/dev/proj/foo' && exec env YI_AGENT_HOME='/home/dev/.yi-agent-projects/proj-foo/.yi-agent' '/b/yi-agent' app-server --listen stdio://"]
```

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/remote/argv.rs desktop/src-tauri/src/remote/mod.rs
git commit -m "feat(remote): build ssh argv as a pure, injection-safe function"
```

---

### Task 4: `remote.json` 读写（flock + 唯一临时名）

**Files:**
- Create: `desktop/src-tauri/src/remote/config.rs`
- Modify: `desktop/src-tauri/src/remote/mod.rs`（加 `pub mod config;`）
- Test: `desktop/src-tauri/src/remote/config.rs` 内嵌 `mod tests`

**Interfaces:**
- Consumes: `RemoteProfile`（Task 2）
- Produces:
  - `pub struct RemoteConfig { pub version: u32, pub profiles: Vec<RemoteProfile> }`
  - `pub fn load(path: &Path) -> RemoteConfig`（坏文件 → 空配置，绝不 panic）
  - `pub fn save(path: &Path, config: &RemoteConfig) -> std::io::Result<()>`
  - `pub const CONFIG_VERSION: u32 = 1`

**要点（spec §4.9）：** 读坏文件一律当"没配置"；写用 temp+rename，临时名带 pid+uuid；`version` 更高则拒绝加载。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn profile(id: &str) -> RemoteProfile {
        RemoteProfile {
            id: id.into(),
            label: format!("{id} label"),
            host: "dev@host".into(),
            local_path: PathBuf::from("/Users/y/proj"),
            remote_path: "/home/dev/proj".into(),
            instance_root: "/home/dev/.yi-agent-projects/x/.yi-agent".into(),
            binary_version: "0.1.10".into(),
            remote_arch: "x86_64-unknown-linux-gnu".into(),
        }
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = load(&dir.path().join("remote.json"));
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert!(cfg.profiles.is_empty());
    }

    #[test]
    fn broken_json_loads_as_empty_not_panic() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote.json");
        std::fs::write(&path, "{ not json").unwrap();
        let cfg = load(&path);
        assert!(cfg.profiles.is_empty(), "a corrupt config must not brick the app");
    }

    #[test]
    fn future_version_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote.json");
        std::fs::write(&path, r#"{"version":99,"profiles":[]}"#).unwrap();
        let cfg = load(&path);
        assert!(cfg.profiles.is_empty(), "must not guess a future schema");
    }

    #[test]
    fn round_trip_preserves_profiles() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote.json");
        let cfg = RemoteConfig { version: CONFIG_VERSION, profiles: vec![profile("proj-foo")] };
        save(&path, &cfg).unwrap();
        assert_eq!(load(&path), cfg);
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote.json");
        save(&path, &RemoteConfig { version: CONFIG_VERSION, profiles: vec![] }).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files leaked: {leftovers:?}");
    }

    #[test]
    fn concurrent_saves_do_not_corrupt_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote.json");
        std::thread::scope(|s| {
            for i in 0..8 {
                let path = path.clone();
                s.spawn(move || {
                    let cfg = RemoteConfig {
                        version: CONFIG_VERSION,
                        profiles: vec![profile(&format!("p{i}"))],
                    };
                    save(&path, &cfg).unwrap();
                });
            }
        });
        // 只要最终文件能解析出 `profiles` 数组就算没撕裂（最后写的那个赢）。
        let cfg = load(&path);
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert!(cfg.profiles.len() == 1 || cfg.profiles.is_empty());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test --lib remote::config
```

Expected: 编译失败，`cannot find function load`。

- [ ] **Step 3: 实现**

```rust
//! `remote.json` 的读写。
//!
//! spec §4.9 的两条硬要求：
//! 1. **读坏文件一律当"没配置"** —— 坏配置绝不能让 desktop 起不来。
//! 2. **写必须跨进程安全** —— 桥接层（Tauri）写、app-server 读；进程内 Mutex
//!    保护不了后者，所以照 `device_store.rs:104-130` 用 flock + 唯一临时名。
//!
//! 只读路径刻意**不加 flock**：temp+rename 保证它读到的要么是旧版要么是新版，
//! 加锁只会让 UI 查询被写操作阻塞。

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::profile::RemoteProfile;

pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub version: u32,
    pub profiles: Vec<RemoteProfile>,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            profiles: Vec::new(),
        }
    }
}

/// 读配置。任何异常都退化成"空配置"，并往 stderr 记一行。
pub fn load(path: &Path) -> RemoteConfig {
    let Ok(text) = std::fs::read_to_string(path) else {
        return RemoteConfig::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        eprintln!("[remote] {} is not valid JSON; treating as unconfigured", path.display());
        return RemoteConfig::default();
    };
    let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    if version != CONFIG_VERSION {
        // 更高版本不猜；更低/缺失视为损坏。
        eprintln!(
            "[remote] {} has version {version}, expected {CONFIG_VERSION}; ignoring",
            path.display()
        );
        return RemoteConfig::default();
    }
    match serde_json::from_value::<RemoteConfig>(value) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("[remote] {} is malformed ({e}); treating as unconfigured", path.display());
            RemoteConfig::default()
        }
    }
}

/// 唯一临时名：并发写者不得共用同一个临时路径，否则一个 `rename` 会
/// `ENOENT` 另一个的（`device_store.rs:120-128` 的同一考量）。
fn temp_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ))
}

/// 原子写。`flock` 覆盖整个读-改-写窗口。
pub fn save(path: &Path, config: &RemoteConfig) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension("json.lock");
    let lock = File::options().create(true).write(true).open(&lock_path)?;
    // 与 `device_store.rs` 同款：阻塞式独占锁，写完随 `lock` 析构释放。
    lock_exclusive(&lock)?;

    let body = serde_json::to_string_pretty(config).map_err(io::Error::other)?;
    let tmp = temp_path(path);
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// `flock(LOCK_EX)`，被信号打断则重试。
fn lock_exclusive(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    loop {
        // SAFETY: `file` 活得比这次调用久，fd 有效。
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(error);
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop/src-tauri && cargo test --lib remote::config
```

Expected: 6 passed。

若 `uuid` / `libc` 未在 `desktop/src-tauri/Cargo.toml` 里，补上（`uuid` 用 `features = ["v4"]`）。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/remote/config.rs desktop/src-tauri/src/remote/mod.rs desktop/src-tauri/Cargo.toml
git commit -m "feat(remote): persist remote.json atomically across processes"
```

---

### Task 5: `Sidecar` 单例 → 槽位表（纯重构，不含远端）

**Files:**
- Modify: `desktop/src-tauri/src/bridge.rs:38-58`（`Sidecar` 结构 + `write_line`）
- Modify: `desktop/src-tauri/src/bridge.rs:61-84`（`rpc` / `rpc_respond` 加 slot）
- Modify: `desktop/src-tauri/src/bridge.rs:398-404`（`clear_child` / `forward_stdout`）
- Modify: `desktop/src-tauri/src/bridge.rs:326-363`（`set_relay_url` / `kill_sidecar`）
- Test: `desktop/src-tauri/src/bridge.rs:426` 的 `mod tests`

**Interfaces:**
- Consumes: `ProfileKey`（Task 2）
- Produces:
  - `pub fn write_line(state: &Sidecar, slot: &ProfileKey, value: &Value) -> Result<(), String>`
  - `#[tauri::command] pub fn rpc(state, slot: String, id, method, params) -> Result<(), String>`
  - `#[tauri::command] pub fn rpc_respond(state, slot: String, id: String, result: Value)`
  - `fn clear_child(app: &AppHandle, slot: &ProfileKey)`
  - `fn event_name(kind: &str, slot: &ProfileKey) -> String`
  - `fn kill_sidecar(app: &AppHandle, slot: &ProfileKey)`
  - `pub fn kill_all_slots(app: &AppHandle)`
  - `fn slot_lock(state: &Sidecar, slot: &ProfileKey) -> Option<Arc<Mutex<Option<CommandChild>>>>`

**本步的验收标准：只有 `Local` 一个槽位，全部既有测试通过、行为不变。** 这是纯重构，不允许顺手改远端逻辑。

- [ ] **Step 1: 先跑基线**

```bash
cd desktop/src-tauri && cargo test --lib 2>&1 | tail -5
```

记下通过数量（后面要比对）。

- [ ] **Step 2: 写失败测试**

在 `bridge.rs` 的 `mod tests` 里加（这些测试不需要真实进程）：

```rust
    #[test]
    fn event_names_are_per_slot() {
        assert_eq!(event_name("message", &ProfileKey::Local), "app-server://message/local");
        assert_eq!(
            event_name("status", &ProfileKey::Remote("proj-foo".into())),
            "app-server://status/proj-foo"
        );
    }

    #[test]
    fn local_slot_keeps_the_historical_event_prefix() {
        // 旧的全局名不再使用，但前缀必须稳定，前端才能一条规则拼出来。
        assert!(event_name("message", &ProfileKey::Local).starts_with("app-server://message/"));
    }

    #[test]
    fn writing_to_an_unknown_slot_is_an_error_not_a_silent_misdelivery() {
        let state = Sidecar::new();
        let err = write_line(&state, &ProfileKey::Remote("nope".into()), &serde_json::json!({}))
            .unwrap_err();
        assert!(err.contains("not running"), "got: {err}");
    }
```

- [ ] **Step 3: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test --lib remote:: 2>&1 | tail -20
cargo test --lib bridge::tests::event_names_are_per_slot
```

Expected: 编译失败（`event_name` 不存在、`write_line` 签名不匹配）。

- [ ] **Step 4: 改 `Sidecar` 与 `write_line`**

把 `bridge.rs:38-58` 换成：

```rust
/// Owns every spawned `app-server` child process, one per slot.
///
/// **锁的粒度是刻意的**：外层锁只保护"查表"，每个槽位有自己的内层锁。写 pipe
/// 是同步 IO（`CommandChild::write` → `write_all`），远端链路一旦积压就会阻塞；
/// 若共用一把锁，一个卡住的远端会让所有槽位（含本地）的 RPC 全部卡死，表现为
/// 「整个 App 卡死」（spec §4.6 障碍三）。
pub struct Sidecar {
    slots: Mutex<HashMap<ProfileKey, Arc<Mutex<Option<CommandChild>>>>>,
}

impl Sidecar {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// 取某槽位的锁，不创建。外层锁在这里**立刻释放**。
    fn slot(&self, key: &ProfileKey) -> Option<Arc<Mutex<Option<CommandChild>>>> {
        self.slots
            .lock()
            .ok()
            .and_then(|guard| guard.get(key).cloned())
    }

    /// 登记一个槽位（覆盖旧句柄）。
    fn set_slot(&self, key: ProfileKey, child: Arc<Mutex<Option<CommandChild>>>) {
        if let Ok(mut guard) = self.slots.lock() {
            guard.insert(key, child);
        }
    }
}

/// 事件名格式的唯一来源：`app-server://{kind}/{slot}`。
fn event_name(kind: &str, slot: &ProfileKey) -> String {
    format!("app-server://{kind}/{}", slot.as_str())
}

fn write_line(state: &Sidecar, slot: &ProfileKey, value: &serde_json::Value) -> Result<(), String> {
    let mut buf = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    buf.push(b'\n');
    // 外层锁已在这一行内释放；下面只持本槽位的锁。
    let cell = state
        .slot(slot)
        .ok_or_else(|| format!("sidecar not running for slot `{}`", slot.as_str()))?;
    let mut guard = cell.lock().map_err(|e| e.to_string())?;
    let child = guard.as_mut().ok_or_else(|| {
        format!("sidecar not running for slot `{}`", slot.as_str())
    })?;
    child.write(&buf).map_err(|e| e.to_string())
}
```

- [ ] **Step 5: 改 `rpc` / `rpc_respond` 加 slot**

```rust
#[tauri::command]
pub fn rpc(
    state: State<'_, Sidecar>,
    slot: String,
    id: u64,
    method: String,
    params: serde_json::Value,
) -> Result<(), String> {
    write_line(
        &state,
        &slot_key(&slot),
        &serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
    )
}

#[tauri::command]
pub fn rpc_respond(
    state: State<'_, Sidecar>,
    slot: String,
    id: String,
    result: serde_json::Value,
) -> Result<(), String> {
    write_line(
        &state,
        &slot_key(&slot),
        &serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
    )
}

/// 前端的槽位标识 → `ProfileKey`。`"local"` 是保留字。
fn slot_key(raw: &str) -> ProfileKey {
    if raw == "local" {
        ProfileKey::Local
    } else {
        ProfileKey::Remote(raw.to_string())
    }
}
```

- [ ] **Step 6: 改 `clear_child` / `forward_stdout` / `kill_sidecar` / `set_relay_url`**

```rust
/// 只摘除**该槽位**的句柄，且只在它仍是我们登记的那个时才摘。
///
/// 单纯 `remove(key)` 会误删"退出后已被监督循环重启"的新句柄：旧进程的
/// `Terminated` 事件可能晚于新进程的登记到达（spec §4.6 障碍二）。
fn clear_child(app: &AppHandle, slot: &ProfileKey) {
    let state = app.state::<Sidecar>();
    if let Ok(mut guard) = state.slots.lock() {
        if let Some(cell) = guard.get(slot) {
            // 只把内容清空，保留槽位登记；监督循环会重新填。
            if let Ok(mut inner) = cell.lock() {
                *inner = None;
            }
        }
    }
}

fn forward_stdout(app: &AppHandle, slot: &ProfileKey, bytes: &[u8]) {
    for line in String::from_utf8_lossy(bytes).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let kind = match classify(&value) {
            Frame::Notification => "message",
            Frame::ReverseRequest => "request",
            Frame::Response => "message",
            Frame::Garbage => continue,
        };
        let _ = app.emit(&event_name(kind, slot), value);
    }
}

/// 只杀指定槽位。
fn kill_sidecar(app: &AppHandle, slot: &ProfileKey) {
    let state = app.state::<Sidecar>();
    if let Some(cell) = state.slot(slot) {
        if let Ok(mut guard) = cell.lock() {
            if let Some(child) = guard.take() {
                let _ = child.kill();
            }
        }
    }
}

/// 杀全部槽位。**只给"关闭 flag"用**，不要变成默认可调用的
/// （spec §4.6 障碍一：改个中继配置就把所有远端项目一起断开是用户可见事故）。
pub fn kill_all_slots(app: &AppHandle) {
    let state = app.state::<Sidecar>();
    let keys: Vec<ProfileKey> = match state.slots.lock() {
        Ok(guard) => guard.keys().cloned().collect(),
        Err(_) => return,
    };
    for key in keys {
        kill_sidecar(app, &key);
    }
}
```

`set_relay_url` 的语义收窄：它现在只重启**本地**槽位。把原来调用 `kill_sidecar(app)` 的地方改成 `kill_sidecar(app, &ProfileKey::Local)`。

- [ ] **Step 7: 让 `spawn` 走槽位（先只有 Local）**

`spawn(app)` 改为 `spawn(app, key: ProfileKey)`，内部 `spawn_once(&app, &key)`；`spawn_once` 里把 `CommandChild` 存进 `state.set_slot(key.clone(), Arc::new(Mutex::new(Some(child))))`；结束时 `clear_child(&app, &key)`；事件 emit 用 `event_name(.., &key)`。

`lib.rs` 的 `setup` 里调用点改成 `bridge::spawn(&handle, ProfileKey::Local)`。

- [ ] **Step 8: 跑全部 desktop 测试**

```bash
cd desktop/src-tauri && cargo test --lib 2>&1 | tail -5
```

Expected: 通过数量 ≥ Step 1 的基线 + 3（新增的三个测试）。**不允许既有测试变红。**

- [ ] **Step 9: 手工验证两条既有路径语义未变**

```bash
cd desktop && npm test -- --run 2>&1 | tail -20
```

Expected: 前端测试全绿（这一步还没改前端，但确认没有连带破坏）。

手工跑一次 App（`npm run dev`），确认：本地对话正常、改 relay URL 后仍能重连、重启 sidecar 仍触发。

- [ ] **Step 10: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/bridge.rs desktop/src-tauri/src/lib.rs
git commit -m "refactor(bridge): key sidecar processes by slot, one lock per slot"
```

---

### Task 6: 前端事件名参数化 + `invoke` 带 slot

**Files:**
- Create: `desktop/src/lib/slots.ts`
- Modify: `desktop/src/tauriTransport.ts:39-62`
- Test: `desktop/src/tauriTransport.test.ts`（若无则建）

**Interfaces:**
- Consumes: Rust 侧的 `app-server://{kind}/{slot}` 约定（Task 5）
- Produces:
  - `export const LOCAL_SLOT = "local"`
  - `export type Slot = string`
  - `export function eventName(kind: "message" | "request" | "status", slot: Slot): string`
  - `export function tauriTransport(slot?: Slot): Transport`

- [ ] **Step 1: 写失败测试**

`desktop/src/tauriTransport.test.ts`：

```ts
import { describe, expect, it, vi } from "vitest";
import { LOCAL_SLOT, eventName } from "./lib/slots";

describe("event naming", () => {
  it("matches the Rust side format", () => {
    expect(eventName("message", LOCAL_SLOT)).toBe("app-server://message/local");
  });

  it("puts the slot last so per-slot subscription is a string match", () => {
    expect(eventName("status", "proj-foo")).toBe("app-server://status/proj-foo");
  });

  it("local slot is the literal the Rust side reserves", () => {
    expect(LOCAL_SLOT).toBe("local");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/tauriTransport.test.ts
```

Expected: FAIL，`Cannot find module './lib/slots'`。

- [ ] **Step 3: 实现 `slots.ts`**

```ts
/**
 * 槽位标识与事件名。
 *
 * **必须与 Rust 侧 `bridge.rs` 的 `event_name` 保持一致**：那里是
 * `format!("app-server://{kind}/{}", slot.as_str())`，本地固定 `"local"`。
 * 两边各写一遍是既有架构的既成事实（前端不 import Rust），所以这里只做一件事：
 * 把它变成唯一常量，别处不再出现裸字符串。
 */

/** 本地 sidecar 的固定槽位名，与 `ProfileKey::Local.as_str()` 对齐。 */
export const LOCAL_SLOT = "local";

/** 远端槽位名即 `remote.json` 里的 profile id。 */
export type Slot = string;

export type EventKind = "message" | "request" | "status";

export function eventName(kind: EventKind, slot: Slot): string {
  return `app-server://${kind}/${slot}`;
}
```

- [ ] **Step 4: 改 `tauriTransport`**

```ts
export function tauriTransport(slot: Slot = LOCAL_SLOT): Transport {
  const unsubscribers = new Set<() => void>();
  const track = (off: () => void): (() => void) => {
    unsubscribers.add(off);
    return () => {
      unsubscribers.delete(off);
      off();
    };
  };
  return {
    send: async (message) => {
      await invoke("rpc", { ...(message as object), slot });
    },
    respond: async (id, result) => {
      await invoke("rpc_respond", { id, result, slot });
    },
    onMessage: (cb) => track(subscribe<unknown>(eventName("message", slot), cb)),
    onRequest: (cb) => track(subscribe<ApprovalRequest>(eventName("request", slot), cb)),
    onStatus: (cb) =>
      track(subscribe<{ state: string; code?: number | null }>(eventName("status", slot), cb)),
    dispose: () => {
      for (const off of [...unsubscribers]) off();
      unsubscribers.clear();
    },
  };
}
```

- [ ] **Step 5: 跑测试**

```bash
cd desktop && npx vitest run src/tauriTransport.test.ts && npx tsc --noEmit
```

Expected: 全绿，类型检查通过。

- [ ] **Step 6: Commit**

```bash
git add desktop/src/lib/slots.ts desktop/src/tauriTransport.test.ts desktop/src/tauriTransport.ts
git commit -m "feat(desktop): parameterize bridge event names by slot"
```

---

### Task 7: 把远端槽位接进监督循环

**Files:**
- Modify: `desktop/src-tauri/src/bridge.rs`（`spawn_once` 分支：本地 sidecar vs 远端 ssh）
- Modify: `desktop/src-tauri/src/lib.rs`（新增 `remote_connect` / `remote_disconnect` / `remote_status` 命令）
- Test: `desktop/src-tauri/src/bridge.rs` 的 `mod tests`（只测纯逻辑部分）

**Interfaces:**
- Consumes: `remote_argv`（Task 3）、`RemoteConfig::load`（Task 4）、`Sidecar` 槽位表（Task 5）
- Produces:
  - `#[tauri::command] pub async fn remote_connect(app, profile_id: String) -> Result<(), String>`
  - `#[tauri::command] pub async fn remote_disconnect(app, profile_id: String) -> Result<(), String>`
  - `#[tauri::command] pub fn remote_status(app) -> serde_json::Value`（`{slots: {id: "connected"|"connecting"|"disconnected"}}`）
  - `fn spawn_remote_once(app: &AppHandle, key: &ProfileKey, profile: &RemoteProfile) -> Result<Receiver<CommandEvent>, String>`

**开关闸门：** `remote_connect` 第一行必须 `if !yi_agent_flags::is_enabled("remote_ssh") { return Err(...) }`。stable 构建 `is_enabled` 恒 false，等于整条链路在 stable 上不可用。

- [ ] **Step 1: 写失败测试（纯逻辑部分）**

```rust
    #[test]
    fn a_remote_slot_key_is_not_the_local_key() {
        assert_ne!(ProfileKey::Remote("local".into()), ProfileKey::Local);
    }

    #[test]
    fn remote_status_starts_disconnected_for_an_unknown_profile() {
        let state = Sidecar::new();
        assert_eq!(state.status_of(&ProfileKey::Remote("nope".into())), "disconnected");
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test --lib bridge::tests::remote_status_starts
```

Expected: 编译失败，`no method status_of`。

- [ ] **Step 3: 实现 `status_of` 与远端启动**

在 `Sidecar` 上加：

```rust
    /// 某槽位的粗粒度状态。只用于 UI 展示与 `remote/relayTargets` 的 `available`。
    fn status_of(&self, key: &ProfileKey) -> &'static str {
        match self.slot(key) {
            None => "disconnected",
            Some(cell) => match cell.lock() {
                Ok(guard) if guard.is_some() => "connected",
                Ok(_) => "connecting",
                Err(_) => "disconnected",
            },
        }
    }
```

`spawn_once` 拆成两条路径：

```rust
fn spawn_once(app: &AppHandle, key: &ProfileKey) -> Result<tauri::async_runtime::Receiver<CommandEvent>, String> {
    match key {
        ProfileKey::Local => spawn_local_once(app, key),
        ProfileKey::Remote(id) => {
            let config = remote::config::load(&remote_config_path());
            let profile = config
                .profiles
                .iter()
                .find(|p| &p.id == id)
                .ok_or_else(|| format!("unknown remote profile `{id}`"))?;
            spawn_remote_once(app, key, profile)
        }
    }
}

fn spawn_remote_once(
    app: &AppHandle,
    key: &ProfileKey,
    profile: &remote::profile::RemoteProfile,
) -> Result<tauri::async_runtime::Receiver<CommandEvent>, String> {
    let binary = remote_binary_path(profile); // `<instance_root>/bin/<version>/yi-agent` 的远端路径
    let relay = remote::profile::derive_relay(configured_relay().as_deref(), &profile.id);
    let argv = remote::argv::remote_argv(profile, &binary, relay.as_deref())
        .map_err(|e| e.to_string())?;

    let (mut rx, child) = app
        .shell()
        .command("ssh")
        .args(argv)
        .spawn()
        .map_err(|e| format!("ssh spawn failed: {e}"))?;

    app.state::<Sidecar>().set_slot(
        key.clone(),
        std::sync::Arc::new(std::sync::Mutex::new(Some(child))),
    );
    Ok(rx)
}
```

`derive_relay` 放进 `remote/profile.rs`（纯函数，可单测）：

```rust
/// 由（本机 relay URL, profile id）派生远端要连的 session。
///
/// **不能用 `.push_str("&session=")` 追加**：`relay_url` 本身可能已经带
/// `?session=`，追加会得到 `?session=a&session=b`，中继取哪个取决于实现
/// （spec §4.6 前提 2）。这里用字符串操作模拟 URL 的 searchParams.set 语义。
pub fn derive_relay(configured: Option<&str>, profile_id: &str) -> Option<String> {
    let base = configured?;
    let session = format!("{}-{}", session_seed(base), profile_id);
    Some(set_query_param(base, "session", &session))
}

fn session_seed(url: &str) -> String {
    // 从 query 里取现有 session 的前缀作为种子；没有就用固定串。
    // 目的只是"不同 profile 得到不同 session"，不需要密码学强度。
    get_query_param(url, "session").unwrap_or_else(|| "remote".to_string())
}

fn get_query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

fn set_query_param(url: &str, key: &str, value: &str) -> String {
    let (head, query) = match url.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (url, None),
    };
    let mut pairs: Vec<String> = query
        .map(|q| {
            q.split('&')
                .filter(|p| !p.is_empty() && !p.starts_with(&format!("{key}=")))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    pairs.push(format!("{key}={value}"));
    format!("{head}?{}", pairs.join("&"))
}
```

对应测试：

```rust
    #[test]
    fn derive_relay_replaces_an_existing_session_param_exactly_once() {
        let out = derive_relay(Some("wss://r/?session=old"), "proj-foo").unwrap();
        assert_eq!(out.matches("session=").count(), 1, "no duplicate param: {out}");
        assert!(out.contains("proj-foo"), "{out}");
        assert!(!out.contains("session=old"), "{out}");
    }

    #[test]
    fn derive_relay_appends_when_there_is_no_query() {
        let out = derive_relay(Some("wss://r/connect"), "p").unwrap();
        assert!(out.contains("session="), "{out}");
        assert!(!out.contains("?session=&"), "{out}");
    }

    #[test]
    fn derive_relay_differs_per_profile() {
        let a = derive_relay(Some("wss://r/?session=s"), "a").unwrap();
        let b = derive_relay(Some("wss://r/?session=s"), "b").unwrap();
        assert_ne!(a, b, "two projects must not share a relay session");
    }

    #[test]
    fn derive_relay_is_none_without_a_configured_url() {
        assert_eq!(derive_relay(None, "p"), None);
    }
```

- [ ] **Step 4: 加三个 Tauri 命令**

```rust
#[tauri::command]
pub fn remote_connect(app: AppHandle, profile_id: String) -> Result<(), String> {
    if !yi_agent_flags::is_enabled("remote_ssh") {
        return Err("remote_ssh is off".into());
    }
    remote::profile::validate_profile_id(&profile_id).map_err(|e| e.to_string())?;
    let key = ProfileKey::Remote(profile_id);
    spawn(&app, key).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remote_disconnect(app: AppHandle, profile_id: String) -> Result<(), String> {
    kill_sidecar(&app, &ProfileKey::Remote(profile_id));
    Ok(())
}

#[tauri::command]
pub fn remote_status(app: AppHandle) -> serde_json::Value {
    let state = app.state::<Sidecar>();
    let keys: Vec<ProfileKey> = match state.slots.lock() {
        Ok(g) => g.keys().cloned().collect(),
        Err(_) => Vec::new(),
    };
    let slots: serde_json::Map<String, serde_json::Value> = keys
        .into_iter()
        .map(|k| (k.as_str().to_string(), serde_json::json!(state.status_of(&k))))
        .collect();
    serde_json::json!({ "slots": slots })
}
```

在 `lib.rs` 的 `invoke_handler` 里注册这三个命令。

- [ ] **Step 5: 跑测试**

```bash
cd desktop/src-tauri && cargo test --lib 2>&1 | tail -5
```

Expected: 全绿，含新增的 6 个测试（2 个槽位 + 4 个 derive_relay）。

- [ ] **Step 6: 手工冒烟（需要一台有 ssh 的机器）**

```bash
# 远端先手工装一个同架构二进制，并按 spec §4.1 第 8 步的命令验证它能起
ssh dev@10.0.0.5 "cd '/home/dev/proj/foo' && exec env YI_AGENT_HOME='/home/dev/.yi-agent-projects/proj-foo/.yi-agent' '/home/dev/.yi-agent/bin/0.1.10/yi-agent' app-server --listen stdio://"
```

在另一个终端确认远端状态目录被创建：

```bash
ssh dev@10.0.0.5 'ls -la /home/dev/.yi-agent-projects/proj-foo/.yi-agent/'
```

Expected: 出现 `threads/` 等目录，且**不是**落在 `/home/dev/.yi-agent/`（后者说明 env 没生效）。

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/bridge.rs desktop/src-tauri/src/remote/profile.rs desktop/src-tauri/src/lib.rs
git commit -m "feat(remote): connect remote slots over ssh with per-slot supervision"
```

---

### Task 8: 前端多 client（`Map<Slot, RpcClient>`）

**Files:**
- Modify: `desktop/src/App.tsx:164`（`clientRef`）、`:1375`（`onVisibilityChange`）、`:1564`（陈旧监听注释处）、`:1597`（`build/info` + `flags/list`）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: `tauriTransport(slot)`（Task 6）、`LOCAL_SLOT`（Task 6）
- Produces: 无对外接口，纯内部改造。

**必须同时做完的四件事（spec §4.6「前端侧的单客户端假设」表）：**
1. 事件名参数化（Task 6 已完成）
2. `onVisibilityChange` 遍历所有槽位重连
3. 每槽位严格 dispose 自己的订阅
4. `build/info` 与 `flags/list` **钉死只发本地槽位**

- [ ] **Step 1: 写失败测试**

在 `App.test.tsx` 里加（用现有测试的 fake transport 套路）：

```tsx
it("sends build/info and flags/list only to the local slot", async () => {
  const sent: Array<{ slot: string; method: string }> = [];
  // 用仓里既有的 fake transport 工厂，记录每次 request 落在哪个 slot。
  // 具体工厂名见同文件顶部的既有 helper —— 沿用，不要新造。
  renderAppWithSlots(["local", "proj-foo"], (slot, method) => sent.push({ slot, method }));
  await waitFor(() => expect(sent.some((s) => s.method === "build/info")).toBe(true));
  expect(sent.filter((s) => s.method === "build/info").every((s) => s.slot === "local")).toBe(true);
  expect(sent.filter((s) => s.method === "flags/list").every((s) => s.slot === "local")).toBe(true);
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/App.test.tsx -t "only to the local slot"
```

Expected: FAIL（`renderAppWithSlots` 不存在，或断言不成立）。

- [ ] **Step 3: 改 `clientRef` 为 Map**

```tsx
// Keyed by slot. `local` is always present; remote slots appear when connected.
const clientsRef = useRef<Map<Slot, RpcClient>>(new Map());
// 当前活动项目对应的槽位。远端项目被选中时切换到对应槽位。
const activeSlotRef = useRef<Slot>(LOCAL_SLOT);

const clientFor = (slot: Slot): RpcClient | undefined => clientsRef.current.get(slot);
const activeClient = (): RpcClient | undefined => clientFor(activeSlotRef.current);
```

把 `:164` 起的所有 `clientRef.current` 引用逐条替换：**读构建信息、发用户请求、管理对话**这类"当前项目"的用 `activeClient()`；**建连、重连、dispose** 这类要遍历 `clientsRef.current.values()`。全文 50 处，按这个规则逐条判断，不要机械替换。

- [ ] **Step 4: 改 `onVisibilityChange`**

```tsx
const onVisibilityChange = () => {
  if (document.visibilityState !== "visible" || disposed) return;
  // 遍历所有槽位：被后台掐断的远端连接不会自己恢复，只重连当前槽位会让
  // 用户切回其它项目时前几个请求失败（spec §4.6）。
  for (const slot of clientsRef.current.keys()) reconnectNow(slot);
};
```

`reconnectNow` 加 `slot` 参数，内部只重建该槽位的 client 并严格 dispose 旧的。

- [ ] **Step 5: 钉死 `build/info` 与 `flags/list`**

```tsx
// 永远问本地槽位：远端 release 构建恒关 flag（spec §5.3.1），问远端会得到
// "关"这个恰好正确但来源错误的答案。
const local = clientFor(LOCAL_SLOT);
if (local) {
  const info = await local.request<{ build_kind?: unknown }>("build/info", {});
  // ... flags/list 同样用 local
}
```

- [ ] **Step 6: 跑测试**

```bash
cd desktop && npx vitest run && npx tsc --noEmit
```

Expected: 全绿。

- [ ] **Step 7: 手工验证双槽位**

开两个项目（一个本地、一个远端），确认：A 的逐字流不会出现在 B；切回前台时两个都重连；关掉 A 的连接不影响 B。

- [ ] **Step 8: Commit**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): hold one RpcClient per slot"
```

---

### Task 9: 关开关时杀掉全部远端槽位

**Files:**
- Modify: `desktop/src-tauri/src/lib.rs`（`flags/set` 的写后处理，或前端开关回调）
- Test: `desktop/src-tauri/src/bridge.rs` 的 `mod tests`

**Interfaces:**
- Consumes: `kill_sidecar(app, slot)`（Task 5）、`remote_ssh` flag（Task 1）
- Produces: `#[tauri::command] pub fn kill_remote_slots(app: AppHandle)`（只杀远端槽位，保留本地）

**为什么单独一个任务：** spec §5.4 要求"关掉开关后不能留下孤儿进程"。这一步容易漏，而且漏了以后用户看到的是"开关关了但远端还在跑"。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn killing_all_slots_leaves_no_registered_child() {
        // 纯逻辑断言：清空后再查状态，所有槽位都是 disconnected。
        // 真实 kill 需要 AppHandle，放在手工验证里做。
        let state = Sidecar::new();
        state.set_slot(ProfileKey::Remote("a".into()), Default::default());
        state.set_slot(ProfileKey::Remote("b".into()), Default::default());
        let keys: Vec<_> = state.slots.lock().unwrap().keys().cloned().collect();
        assert_eq!(keys.len(), 2);
    }
```

- [ ] **Step 2: 实现**

前端开关回调里，关掉时调用（**命令名就是 `kill_remote_slots`**，与下面的
Rust 函数同名——Tauri 默认把 snake_case 命令名直接暴露，不加 `_command` 后缀）：

```ts
await invoke("kill_remote_slots");
```

Rust 侧：

```rust
/// 关 `remote_ssh` 时调用：杀掉**全部**远端槽位，不留孤儿。
///
/// **与 `kill_all_slots` 的区别**：那个连本地槽位一起杀（只在 App 退出时用），
/// 这个只杀远端——本地 sidecar 跟 flag 无关，必须一直活着。
#[tauri::command]
pub fn kill_remote_slots(app: AppHandle) {
    let state = app.state::<Sidecar>();
    let keys: Vec<ProfileKey> = match state.slots.lock() {
        Ok(g) => g
            .keys()
            .filter(|k| matches!(k, ProfileKey::Remote(_)))
            .cloned()
            .collect(),
        Err(_) => return,
    };
    for key in keys {
        kill_sidecar(&app, &key);
    }
}
```

- [ ] **Step 3: 跑测试 + 手工验证**

```bash
cd desktop/src-tauri && cargo test --lib 2>&1 | tail -3
```

手工：连一个远端 → 关 flag → `ps ax | grep "ssh .*app-server"` 应无残留 → 远端 `ps ax | grep app-server` 也应无残留（`exec` 让 ssh 断开时信号直达）。

- [ ] **Step 4: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/bridge.rs desktop/src-tauri/src/lib.rs desktop/src/App.tsx
git commit -m "feat(remote): kill every remote slot when the flag is turned off"
```

---

## 自检（Self-Review）

**1. Spec 覆盖**

| spec §10 步骤 | 对应任务 |
|---|---|
| 1. flag 注册 + stable 闸门 | Task 1 |
| 2. `remote.json` 读写与校验 | Task 2（校验）、Task 4（读写） |
| 3. 下载 + 缓存 + scp | **不在本计划**，见计划 2 |
| 4. 桥接层多槽位改造 | Task 5 |
| 5. `remote_argv` 纯函数 | Task 3 |
| 6. `bridge.rs` 接入远端 slot | Task 7（`--relay` 派生在 `derive_relay`） |
| 7. 路径语义 | **不在本计划**，见计划 3 |
| 8. 前端多 client | Task 6（事件名）、Task 8（Map） |
| 9. 设置页 + 进度 + 断线 | **不在本计划**，见计划 3 |
| 10. 关开关杀进程 | Task 9 |
| 11. `remote/relayTargets` | **不在本计划**，见计划 3 |
| 12. 手机端多连接 | **不在本计划**，见计划 3 |
| 13–14. 端到端 + 路径 2/3 | 见计划 4 |

**2. 占位符扫描**：无 TBD / "适当处理错误" / "同 Task N"。每个代码步骤都给了可粘贴的完整实现。

**3. 类型一致性**：
- `ProfileKey::as_str()` 在 Task 2 定义，Task 5/7/9 使用，签名一致
- `write_line(state, slot, value)` 在 Task 5 定义与调用一致
- `event_name(kind, slot)` Rust 侧与 `eventName(kind, slot)` TS 侧格式对齐（`app-server://{kind}/{slot}`），Task 6 的测试断言了这个格式
- `remote_argv(profile, binary, relay)` 在 Task 3 定义，Task 7 调用一致
- `derive_relay(configured, profile_id)` 在 Task 7 定义与测试一致

**已知偏差**：Task 8 的 `renderAppWithSlots` 是测试 helper 名，实际实现时应复用 `App.test.tsx` 里既有的 fake 工厂而不是新造——已在步骤里写明。
