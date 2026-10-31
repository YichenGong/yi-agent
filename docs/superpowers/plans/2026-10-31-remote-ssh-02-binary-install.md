# 远程 SSH 执行 · 计划 2：远端二进制安装与缓存

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用户第一次连接远端项目时，把与远端架构匹配的 `yi-agent` 二进制自动装到远端 `~/.yi-agent/bin/<version>/`，本机侧缓存下载产物，全程无手工步骤。

**Architecture:** 三条路径按优先级：①本机下载 → 校验 sha256 → scp → 远端 `tar` 解压（主路径）；②远端自己去发布渠道下载（远端有外网时省本机带宽）；③用户 `--from` 指定本地文件（气隙/不支持架构）。安装前先探测远端 `uname -sm` 与 `tar` 能力，结果缓存进 profile，避免每次重连都探测。

**Tech Stack:** Rust（`reqwest` blocking + rustls、`sha2`）、系统 `ssh`/`scp`/`tar`、GitHub Releases 作为发布渠道。

**依据 spec:** `docs/superpowers/specs/2026-10-31-remote-ssh-execution-design.md` §2.4（全部小节）、§4.9（`binary_version` / `remote_arch` 字段）、§8.2。

## Global Constraints

- **支持矩阵只有三个 target**（spec §2.4.5）：`aarch64-apple-darwin`、`x86_64-apple-darwin`、`x86_64-unknown-linux-gnu`。其余**必须显式拒绝**，错误文案要说明"该架构没有预编译产物"，不能说"网络失败"。
- **产物命名固定**：`yi-agent-<target>.tar.gz` 与 `yi-agent-<target>.tar.gz.sha256`，来自 `justfile` 的 `package-target`。
- **下载地址**：`https://github.com/gongyichen/yi-agent/releases/download/v<version>/<artifact>`。
- **绝不 scp 本机二进制给不同架构的远端**（spec §2.4.4）：本机是 Mach-O，Linux 是 ELF。
- **安装路径** `~/.yi-agent/bin/<version>/yi-agent`，按版本分目录，不覆盖（spec §2.5）。
- **`tar -xzf` 不保证可用**（spec §2.4.2）：GNU tar 的 `-z` 要 fork 外部 `gzip`，极简镜像可能没有。必须探测后降级为未压缩 tar。
- **一切下载都必须校验 sha256**，校验失败即中止，不落盘到安装位。
- **本地缓存**在 `~/.yi-agent/remote-cache/<version>/`，跨 App 重装存活（spec §2.4.2）。
- **单测不联网**：下载逻辑抽成 `trait Fetcher`，测试注入假实现。

---

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `desktop/src-tauri/src/remote/arch.rs` | `uname -sm` 输出 → 受支持 target 的归一化；不支持时给可读错误 | 建 |
| `desktop/src-tauri/src/remote/cache.rs` | 本机下载缓存：路径推导、命中判断、原子落盘 | 建 |
| `desktop/src-tauri/src/remote/fetch.rs` | `Fetcher` trait + `reqwest` 实现 + sha256 校验 | 建 |
| `desktop/src-tauri/src/remote/probe.rs` | 探测远端 `uname` 与 `tar` 能力（纯解析 + 一个 ssh 执行封装） | 建 |
| `desktop/src-tauri/src/remote/install.rs` | 编排三条路径；`InstallPlan` 决策表 | 建 |
| `desktop/src-tauri/src/remote/mod.rs` | 加四个 `pub mod` | 改 |
| `desktop/src-tauri/Cargo.toml` | 加 `reqwest`、`sha2` | 改 |

**为什么 `arch.rs` 和 `probe.rs` 分开**：前者是"字符串 → target"的纯映射（可穷举测试所有平台），后者要跑 ssh（有 IO）。混在一起会让纯映射的测试也得起进程。

**为什么 `install.rs` 只编排不实现**：三条路径的决策逻辑（哪条先、失败怎么降级）是最容易出错的部分，单独成文件后可以用假 `Fetcher` + 假 ssh 把每条分支都走一遍，不需要真的下载 15MB。

---

### Task 1: 架构归一化（纯映射）

**Files:**
- Create: `desktop/src-tauri/src/remote/arch.rs`
- Modify: `desktop/src-tauri/src/remote/mod.rs`
- Test: `arch.rs` 内嵌 `mod tests`

**Interfaces:**
- Consumes: 无
- Produces:
  - `pub fn normalize_target(uname_sm: &str) -> Result<&'static str, ArchError>`
  - `pub enum ArchError { Unsupported { os: String, machine: String }, Unparsable(String) }`，`impl Display` 给出两条出路

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_x86_64_maps_to_the_gnu_target() {
        assert_eq!(normalize_target("Linux x86_64").unwrap(), "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn macOS_arm64_maps_to_aarch64_darwin() {
        // 注意：macOS 的 uname -m 是 `arm64`，不是 Rust target 里的 `aarch64`。
        assert_eq!(normalize_target("Darwin arm64").unwrap(), "aarch64-apple-darwin");
    }

    #[test]
    fn macOS_x86_64_maps_to_x86_64_darwin() {
        assert_eq!(normalize_target("Darwin x86_64").unwrap(), "x86_64-apple-darwin");
    }

    #[test]
    fn arm_linux_is_rejected_with_an_actionable_message() {
        let err = normalize_target("Linux aarch64").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("aarch64"), "must name the arch: {msg}");
        assert!(
            msg.contains("--from") || msg.contains("x86_64"),
            "must offer a way out: {msg}"
        );
        assert!(
            !msg.contains("network") && !msg.contains("网络"),
            "must NOT look like a network problem: {msg}"
        );
    }

    #[test]
    fn musl_and_windows_are_rejected() {
        assert!(normalize_target("Linux x86_64").is_ok());
        assert!(normalize_target("Windows x86_64").is_err());
        assert!(normalize_target("FreeBSD amd64").is_err());
    }

    #[test]
    fn garbage_is_unparsable_not_unsupported() {
        assert!(matches!(normalize_target(""), Err(ArchError::Unparsable(_))));
        assert!(matches!(normalize_target("Linux"), Err(ArchError::Unparsable(_))));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test --lib remote::arch
```

Expected: 编译失败，`cannot find function normalize_target`。

- [ ] **Step 3: 实现**

```rust
//! 远端架构 → 受支持的预编译 target。
//!
//! 发布矩阵只有三个 target（spec §2.4.5）。**不支持时必须显式拒绝**，且文案要
//! 指向真实原因——"该架构没有预编译产物"，而不是含糊的"下载失败"。用户看到
//! "下载失败"会去查网络，查一天也查不出来。

use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum ArchError {
    /// 能解析，但不在发布矩阵里。
    Unsupported { os: String, machine: String },
    /// `uname -sm` 的输出看不出来是什么。
    Unparsable(String),
}

impl fmt::Display for ArchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArchError::Unsupported { os, machine } => write!(
                f,
                "remote architecture `{os} {machine}` has no prebuilt yi-agent binary \
                 (only aarch64-apple-darwin, x86_64-apple-darwin and \
                 x86_64-unknown-linux-gnu are published). Run the project on an \
                 x86_64 Linux or macOS host, or install a binary you built yourself \
                 with `--from <path>`."
            ),
            ArchError::Unparsable(raw) => {
                write!(f, "could not parse `uname -sm` output: `{raw}`")
            }
        }
    }
}

/// `uname -sm` 的两段（OS、machine）→ Rust target triple。
///
/// macOS 的机器名是 `arm64` 而 Rust 叫 `aarch64`，这处不一致是最常见的错。
pub fn normalize_target(uname_sm: &str) -> Result<&'static str, ArchError> {
    let mut parts = uname_sm.split_whitespace();
    let (Some(os), Some(machine)) = (parts.next(), parts.next()) else {
        return Err(ArchError::Unparsable(uname_sm.trim().to_string()));
    };
    if parts.next().is_some() {
        // `uname -sm` 只应有两段；多出来说明调用错了（比如传了 `uname -a`）。
        return Err(ArchError::Unparsable(uname_sm.trim().to_string()));
    }
    let os = os.to_ascii_lowercase();
    let machine = machine.to_ascii_lowercase();
    match (os.as_str(), machine.as_str()) {
        ("linux", "x86_64") | ("linux", "amd64") => Ok("x86_64-unknown-linux-gnu"),
        ("darwin", "arm64") | ("darwin", "aarch64") => Ok("aarch64-apple-darwin"),
        ("darwin", "x86_64") => Ok("x86_64-apple-darwin"),
        _ => Err(ArchError::Unsupported {
            os: os.to_string(),
            machine: machine.to_string(),
        }),
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop/src-tauri && cargo test --lib remote::arch
```

Expected: 6 passed。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/remote/arch.rs desktop/src-tauri/src/remote/mod.rs
git commit -m "feat(remote): map uname output to the three supported targets"
```

---

### Task 2: `Fetcher` trait 与 sha256 校验

**Files:**
- Create: `desktop/src-tauri/src/remote/fetch.rs`
- Modify: `desktop/src-tauri/src/remote/mod.rs`
- Modify: `desktop/src-tauri/Cargo.toml`
- Test: `fetch.rs` 内嵌 `mod tests`（用假 Fetcher，不联网）

**Interfaces:**
- Consumes: 无
- Produces:
  - `pub trait Fetcher { fn get(&self, url: &str) -> Result<Vec<u8>, String>; }`
  - `pub struct HttpFetcher;`（`reqwest::blocking` 实现）
  - `pub fn download_verified(fetcher: &dyn Fetcher, base: &str, target: &str, version: &str, dest: &Path) -> Result<PathBuf, String>`
  - `pub fn sha256_hex(bytes: &[u8]) -> String`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeFetcher {
        responses: RefCell<Vec<(String, Vec<u8>)>>,
    }

    impl FakeFetcher {
        fn new(pairs: &[(&str, &[u8])]) -> Self {
            Self {
                responses: RefCell::new(
                    pairs.iter().map(|(u, b)| (u.to_string(), b.to_vec())).collect(),
                ),
            }
        }
    }

    impl Fetcher for FakeFetcher {
        fn get(&self, url: &str) -> Result<Vec<u8>, String> {
            self.responses
                .borrow()
                .iter()
                .find(|(u, _)| u == url)
                .map(|(_, b)| b.clone())
                .ok_or_else(|| format!("404 {url}"))
        }
    }

    const TARBALL: &[u8] = b"pretend tarball bytes";

    fn sha(bytes: &[u8]) -> String {
        sha256_hex(bytes)
    }

    #[test]
    fn sha256_matches_a_known_vector() {
        // "abc" 的 sha256，标准测试向量。
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_matching_checksum_writes_the_artifact() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = "https://example.test/v1";
        let target = "x86_64-unknown-linux-gnu";
        let tar_url = format!("{base}/yi-agent-{target}.tar.gz");
        let sha_url = format!("{tar_url}.sha256");
        let f = FakeFetcher::new(&[
            (&tar_url, TARBALL),
            (&sha_url, format!("  {}\n", sha(TARBALL)).as_bytes()),
        ]);
        let dest = dir.path().join("out.tar.gz");
        let got = download_verified(&f, base, target, "0.1.10", &dest).unwrap();
        assert_eq!(got, dest);
        assert_eq!(std::fs::read(&dest).unwrap(), TARBALL);
    }

    #[test]
    fn checksum_with_trailing_newline_and_spaces_still_parses() {
        // 真实的 `.sha256` 是 `shasum -a 256` 的产物，可能带文件名与换行。
        let dir = tempfile::TempDir::new().unwrap();
        let base = "https://example.test/v1";
        let target = "x86_64-unknown-linux-gnu";
        let tar_url = format!("{base}/yi-agent-{target}.tar.gz");
        let sha_url = format!("{tar_url}.sha256");
        let body = format!("{}  yi-agent-{target}.tar.gz\n", sha(TARBALL));
        let f = FakeFetcher::new(&[(&tar_url, TARBALL), (&sha_url, body.as_bytes())]);
        download_verified(&f, base, target, "0.1.10", &dir.path().join("o.tar.gz")).unwrap();
    }

    #[test]
    fn a_mismatched_checksum_never_writes_the_artifact() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = "https://example.test/v1";
        let target = "x86_64-unknown-linux-gnu";
        let tar_url = format!("{base}/yi-agent-{target}.tar.gz");
        let sha_url = format!("{tar_url}.sha256");
        let f = FakeFetcher::new(&[
            (&tar_url, TARBALL),
            (&sha_url, b"0000000000000000000000000000000000000000000000000000000000000000"),
        ]);
        let dest = dir.path().join("out.tar.gz");
        assert!(download_verified(&f, base, target, "0.1.10", &dest).is_err());
        assert!(!dest.exists(), "a bad checksum must not leave the file behind");
    }

    #[test]
    fn a_missing_artifact_surfaces_the_http_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = FakeFetcher::new(&[]);
        let err = download_verified(
            &f,
            "https://example.test/v1",
            "x86_64-unknown-linux-gnu",
            "0.1.10",
            &dir.path().join("o.tar.gz"),
        )
        .unwrap_err();
        assert!(err.contains("404"), "{err}");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test --lib remote::fetch
```

Expected: 编译失败（`Fetcher` 不存在）。

- [ ] **Step 3: 实现**

```rust
//! 下载远端二进制并校验。
//!
//! `Fetcher` 是注入点：生产用 `HttpFetcher`，测试用假实现，
//! 这样"校验失败不落盘"这类关键分支不需要真的下载 15MB。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub const RELEASE_BASE: &str = "https://github.com/gongyichen/yi-agent/releases/download";

/// 取一个 URL 的全部字节。只用于小文件（tar.gz 与它的 sha256）。
pub trait Fetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// 生产实现。`blocking` 是刻意的：安装流程在 Tauri 命令里跑，调用方自己
/// `spawn_blocking`；这里再套一层 async 只会让测试变复杂。
pub struct HttpFetcher;

impl Fetcher for HttpFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        let response = reqwest::blocking::get(url).map_err(|e| format!("{url}: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("{url}: HTTP {}", response.status()));
        }
        response.bytes().map(|b| b.to_vec()).map_err(|e| format!("{url}: {e}"))
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// `.sha256` 文件的第一段十六进制。真实的文件可能带文件名与换行。
fn parse_checksum(raw: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(raw);
    let token = text
        .split_whitespace()
        .next()
        .ok_or_else(|| "the .sha256 file is empty".to_string())?;
    if token.len() != 64 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("the .sha256 file is malformed: `{token}`"));
    }
    Ok(token.to_ascii_lowercase())
}

/// 下载 `yi-agent-<target>.tar.gz` 到 `dest`，核对同名 `.sha256`。
///
/// **先校验再落盘**：校验不过时 `dest` 不存在，避免调用方误用一个坏文件。
pub fn download_verified(
    fetcher: &dyn Fetcher,
    base: &str,
    target: &str,
    version: &str,
    dest: &Path,
) -> Result<PathBuf, String> {
    let tar_url = format!("{base}/v{version}/yi-agent-{target}.tar.gz");
    let sha_url = format!("{tar_url}.sha256");

    let expected = parse_checksum(&fetcher.get(&sha_url)?)?;
    let bytes = fetcher.get(&tar_url)?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        return Err(format!(
            "checksum mismatch for {tar_url}: expected {expected}, got {actual}"
        ));
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // 唯一临时名 + rename，沿用 `device_store.rs:120-128` 的考量。
    let tmp = dest.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
    Ok(dest.to_path_buf())
}
```

`Cargo.toml` 加：

```toml
sha2 = "0.10"
reqwest = { version = "0.12", default-features = false, features = ["blocking", "rustls-tls"] }
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop/src-tauri && cargo test --lib remote::fetch
```

Expected: 5 passed。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/remote/fetch.rs desktop/src-tauri/src/remote/mod.rs desktop/src-tauri/Cargo.toml
git commit -m "feat(remote): download and sha256-verify release artifacts"
```

---

### Task 3: 探测远端能力（`uname` + `tar` 是否支持 gzip）

**Files:**
- Create: `desktop/src-tauri/src/remote/probe.rs`
- Modify: `desktop/src-tauri/src/remote/mod.rs`
- Test: `probe.rs` 内嵌 `mod tests`

**Interfaces:**
- Consumes: `normalize_target`（Task 1）
- Produces:
  - `pub struct RemoteCaps { pub target: &'static str, pub tar_supports_gzip: bool, pub has_tar: bool }`
  - `pub fn parse_caps(uname_out: &str, tar_probe_out: &str) -> Result<RemoteCaps, String>`
  - `pub fn probe_command() -> &'static str`（返回要跑的远端命令串）
  - `pub fn expand_home_probe_command() -> &'static str`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_capable_tar_is_detected() {
        let caps = parse_caps("Linux x86_64\n", "gzip\n", ).unwrap();
        assert_eq!(caps.target, "x86_64-unknown-linux-gnu");
        assert!(caps.tar_supports_gzip);
        assert!(caps.has_tar);
    }

    #[test]
    fn plain_tar_is_detected() {
        let caps = parse_caps("Linux x86_64\n", "plain\n").unwrap();
        assert!(!caps.tar_supports_gzip);
        assert!(caps.has_tar);
    }

    #[test]
    fn missing_tar_is_detected() {
        let caps = parse_caps("Linux x86_64\n", "none\n").unwrap();
        assert!(!caps.has_tar);
    }

    #[test]
    fn caps_are_line_ordered_specific_then_generic() {
        // 真实输出可能是 `gzip` 前面还有别的行（比如 shell 的 motd）。
        let caps = parse_caps("warning: something\nLinux x86_64\n", "noise\nplain\n").unwrap();
        // uname 只取最后一行含两段的那行
        assert_eq!(caps.target, "x86_64-unknown-linux-gnu");
        assert!(!caps.tar_supports_gzip);
    }

    #[test]
    fn unsupported_arch_fails_at_probe_time() {
        // spec §2.4.5：连接前就失败，不能先 scp 再发现不对。
        assert!(parse_caps("Linux aarch64\n", "gzip\n").is_err());
    }

    #[test]
    fn the_probe_command_asks_uname_and_tar_capability() {
        let cmd = probe_command();
        assert!(cmd.contains("uname -sm"), "{cmd}");
        assert!(cmd.contains("tar"), "{cmd}");
        assert!(cmd.contains("gzip"), "{cmd}");
        // 必须是一条能一次跑完的命令（一次 ssh 往返，不是三次）。
        assert!(!cmd.contains("ssh "), "the command must not nest ssh: {cmd}");
    }

    #[test]
    fn the_home_probe_does_not_assume_home_is_set() {
        let cmd = expand_home_probe_command();
        assert!(cmd.contains("HOME") || cmd.contains("~"), "{cmd}");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test --lib remote::probe
```

Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
//! 探测远端能不能装、怎么解压。
//!
//! **一次 ssh 往返**问完所有事：每次往返几百毫秒到几秒，串行问三次会让
//! "首次连接"多等好几秒。所以 `probe_command()` 刻意拼成一条命令。

use crate::remote::arch::normalize_target;

#[derive(Debug, PartialEq, Eq)]
pub struct RemoteCaps {
    pub target: &'static str,
    /// `tar -xzf` 能不能用。等于"有 tar 且 tar 能找到 gzip"。
    pub tar_supports_gzip: bool,
    pub has_tar: bool,
}

/// 一次 ssh 跑完：先 uname，再试探 tar。
///
/// 输出约定（每行一个标记，解析时按标记找，不按行号）：
/// - `YI_UNAME=<uname -sm>`
/// - `YI_TAR=gzip` / `YI_TAR=plain` / `YI_TAR=none`
pub fn probe_command() -> &'static str {
    concat!(
        "echo YI_UNAME=$(uname -sm); ",
        "if ! command -v tar >/dev/null 2>&1; then echo YI_TAR=none; ",
        "elif tar --help 2>&1 | grep -qi gzip; then echo YI_TAR=gzip; ",
        "else echo YI_TAR=plain; fi"
    )
}

/// 远端 home 的绝对路径。`~` 在非交互 ssh 里通常可用，但不保证；
/// `$HOME` 更可靠，两者都试。
pub fn expand_home_probe_command() -> &'static str {
    r#"printf '%s\n' "${HOME:-$(cd ~ && pwd)}""#
}

fn tagged<'a>(output: &'a str, tag: &str) -> Option<&'a str> {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix(tag))
        .map(str::trim)
}

/// 解析探测输出。**不按行号**——远端 shell 的 motd/警告会混进来。
pub fn parse_caps(uname_out: &str, tar_probe_out: &str) -> Result<RemoteCaps, String> {
    let uname = tagged(uname_out, "YI_UNAME=")
        .ok_or_else(|| format!("the remote probe printed no YI_UNAME line:\n{uname_out}"))?;
    // 支持矩阵外的架构在这里就失败，而不是等下载 404。
    let target = normalize_target(uname).map_err(|e| e.to_string())?;

    let tar = tagged(tar_probe_out, "YI_TAR=")
        .ok_or_else(|| format!("the remote probe printed no YI_TAR line:\n{tar_probe_out}"))?;
    let (has_tar, tar_supports_gzip) = match tar {
        "gzip" => (true, true),
        "plain" => (true, false),
        "none" => (false, false),
        other => return Err(format!("unexpected YI_TAR value `{other}`")),
    };
    Ok(RemoteCaps {
        target,
        tar_supports_gzip,
        has_tar,
    })
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop/src-tauri && cargo test --lib remote::probe
```

Expected: 7 passed。

- [ ] **Step 5: 手工在真实远端跑一次探测**

```bash
ssh dev@10.0.0.5 'echo YI_UNAME=$(uname -sm); if ! command -v tar >/dev/null 2>&1; then echo YI_TAR=none; elif tar --help 2>&1 | grep -qi gzip; then echo YI_TAR=gzip; else echo YI_TAR=plain; fi'
```

Expected: 两行 `YI_UNAME=Linux x86_64` / `YI_TAR=gzip`。

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/remote/probe.rs desktop/src-tauri/src/remote/mod.rs
git commit -m "feat(remote): probe arch and tar capability in one ssh round-trip"
```

---

### Task 4: 本机缓存与安装路径派生（纯逻辑）

**Files:**
- Create: `desktop/src-tauri/src/remote/cache.rs`
- Modify: `desktop/src-tauri/src/remote/mod.rs`
- Test: `cache.rs` 内嵌 `mod tests`

**Interfaces:**
- Consumes: 无
- Produces:
  - `pub fn cache_path(home: &Path, version: &str, target: &str) -> PathBuf`
  - `pub fn remote_binary_path(instance_root: &str, version: &str) -> String`
  - `pub fn remote_install_dir(instance_root: &str, version: &str) -> String`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn cache_lives_under_the_instance_home() {
        let p = cache_path(Path::new("/Users/y/.yi-agent"), "0.1.10", "x86_64-unknown-linux-gnu");
        assert_eq!(
            p,
            PathBuf::from("/Users/y/.yi-agent/remote-cache/0.1.10/yi-agent-x86_64-unknown-linux-gnu.tar.gz")
        );
    }

    #[test]
    fn different_versions_do_not_collide() {
        let a = cache_path(Path::new("/h"), "0.1.9", "t");
        let b = cache_path(Path::new("/h"), "0.1.10", "t");
        assert_ne!(a, b);
    }

    #[test]
    fn remote_binary_is_versioned_not_overwritten() {
        // spec §2.5：按版本分目录，多版本并存不打架。
        assert_eq!(
            remote_binary_path("/home/dev/.yi-agent-projects/p/.yi-agent", "0.1.10"),
            "/home/dev/.yi-agent-projects/p/.yi-agent/bin/0.1.10/yi-agent"
        );
    }

    #[test]
    fn install_dir_is_the_parent_of_the_binary() {
        assert_eq!(
            remote_install_dir("/home/dev/.yi-agent-projects/p/.yi-agent", "0.1.10"),
            "/home/dev/.yi-agent-projects/p/.yi-agent/bin/0.1.10"
        );
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test --lib remote::cache
```

Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
//! 安装位置与缓存的路径派生。
//!
//! 全是纯字符串/路径拼接，没有 IO——所以可以穷举测试。实际写盘在
//! `fetch.rs`（本机）与 `install.rs`（远端，经 ssh）。

use std::path::{Path, PathBuf};

/// 本机缓存。**放在 instance_home 下**，跨 App 重装存活（spec §2.4.2）。
pub fn cache_path(home: &Path, version: &str, target: &str) -> PathBuf {
    home.join("remote-cache")
        .join(version)
        .join(format!("yi-agent-{target}.tar.gz"))
}

/// 远端二进制路径。按版本分目录（spec §2.5）。
pub fn remote_binary_path(instance_root: &str, version: &str) -> String {
    format!("{instance_root}/bin/{version}/yi-agent")
}

/// 远端的版本目录，解压目标。
pub fn remote_install_dir(instance_root: &str, version: &str) -> String {
    format!("{instance_root}/bin/{version}")
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop/src-tauri && cargo test --lib remote::cache
```

Expected: 4 passed。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/remote/cache.rs desktop/src-tauri/src/remote/mod.rs
git commit -m "feat(remote): derive cache and remote install paths"
```

---

### Task 5: 安装编排（三条路径 + 降级决策表）

**Files:**
- Create: `desktop/src-tauri/src/remote/install.rs`
- Modify: `desktop/src-tauri/src/remote/mod.rs`
- Test: `install.rs` 内嵌 `mod tests`

**Interfaces:**
- Consumes: Task 1–4 的全部产出
- Produces:
  - `pub enum InstallRoute { LocalDownload, RemoteDownload, FromFile }`
  - `pub fn choose_route(caps: &RemoteCaps, has_cache: bool, from: Option<&Path>) -> InstallRoute`
  - `pub fn unpack_command(caps: &RemoteCaps, tarball_remote: &str, dest: &str) -> String`
  - `pub struct Plan { pub route: InstallRoute, pub unpack: String }`
  - `pub fn plan(caps, has_cache, from, tarball_remote, dest) -> Plan`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::probe::RemoteCaps;

    fn caps(gzip: bool, has_tar: bool) -> RemoteCaps {
        RemoteCaps { target: "x86_64-unknown-linux-gnu", tar_supports_gzip: gzip, has_tar }
    }

    #[test]
    fn from_file_wins_when_given() {
        // 显式动作优先：用户给了文件就别去下载。
        let route = choose_route(&caps(true, true), false, Some(Path::new("/tmp/yi-agent")));
        assert_eq!(route, InstallRoute::FromFile);
    }

    #[test]
    fn cache_hit_uses_the_local_download_route() {
        let route = choose_route(&caps(true, true), true, None);
        assert_eq!(route, InstallRoute::LocalDownload);
    }

    #[test]
    fn no_cache_and_no_tar_falls_back_to_remote_download() {
        // 没有 tar 就没法解压，只能让远端自己拉并当场执行（路径 2）。
        let route = choose_route(&caps(false, false), false, None);
        assert_eq!(route, InstallRoute::RemoteDownload);
    }

    #[test]
    fn gzip_unpack_uses_xzf() {
        let cmd = unpack_command(&caps(true, true), "/tmp/a.tar.gz", "/home/dev/.yi-agent/bin/1");
        assert!(cmd.contains("tar -xzf"), "{cmd}");
        assert!(cmd.contains("-C '/home/dev/.yi-agent/bin/1'"), "{cmd}");
    }

    #[test]
    fn plain_unpack_uses_xf_without_z() {
        // GNU tar 的 -z 要外部 gzip；没有就绝不能用 -z。
        let cmd = unpack_command(&caps(false, true), "/tmp/a.tar", "/home/dev/b");
        assert!(cmd.contains("tar -xf "), "{cmd}");
        assert!(!cmd.contains("-xzf"), "{cmd}");
    }

    #[test]
    fn plain_route_tarball_is_not_gz_suffixed() {
        let p = plan(&caps(false, true), false, None, "/tmp/yi-agent.tar", "/home/d/b");
        assert!(p.unpack.contains("/tmp/yi-agent.tar"), "{}", p.unpack);
        assert!(!p.unpack.contains(".tar.gz "), "{}", p.unpack);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test --lib remote::install
```

Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
//! 安装编排：选路 + 拼远端解压命令。
//!
//! 刻意只做决策，不做 IO——这样每条降级分支都能在单测里走一遍，
//! 不用真的下载 15MB 或连一台没有 gzip 的机器。

use std::path::Path;

use crate::remote::probe::RemoteCaps;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum InstallRoute {
    /// 本机下载（或命中缓存）→ scp → 远端解压。主路径。
    LocalDownload,
    /// 远端自己 curl/wget。远端有外网时用；也是"远端没有 tar"时的兜底。
    RemoteDownload,
    /// 用户用 `--from` 指定的本地文件。气隙环境与不支持架构的出路。
    FromFile,
}

pub struct Plan {
    pub route: InstallRoute,
    pub unpack: String,
}

/// 选路。顺序即优先级：
/// 1. `--from` 给了就用（显式动作不该被"我有缓存"覆盖）
/// 2. 否则本机下载（缓存命中与否只影响是否重新下载，不影响选路）
/// 3. 远端连 `tar` 都没有 → 只能远端下载，且下载后必须能直接执行
pub fn choose_route(caps: &RemoteCaps, _has_cache: bool, from: Option<&Path>) -> InstallRoute {
    if from.is_some() {
        return InstallRoute::FromFile;
    }
    if !caps.has_tar {
        return InstallRoute::RemoteDownload;
    }
    InstallRoute::LocalDownload
}

/// 远端解压命令。
///
/// **`-z` 只在确认远端 gzip 可用时才加**：GNU tar 的 `-z` 会 fork 外部 `gzip`，
/// 极简镜像里没有它，`tar -xzf` 直接失败（spec §2.4.2）。
pub fn unpack_command(caps: &RemoteCaps, tarball_remote: &str, dest: &str) -> String {
    let flag = if caps.tar_supports_gzip { "-xzf" } else { "-xf" };
    // 目录必须先建：`tar -C` 不会创建目标目录。
    format!(
        "mkdir -p '{}' && tar {} '{}' -C '{}'",
        dest.replace('\'', r"'\''"),
        flag,
        tarball_remote.replace('\'', r"'\''"),
        dest.replace('\'', r"'\''")
    )
}

pub fn plan(
    caps: &RemoteCaps,
    has_cache: bool,
    from: Option<&Path>,
    tarball_remote: &str,
    dest: &str,
) -> Plan {
    Plan {
        route: choose_route(caps, has_cache, from),
        unpack: unpack_command(caps, tarball_remote, dest),
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop/src-tauri && cargo test --lib remote::install
```

Expected: 6 passed。

- [ ] **Step 5: 接上真实执行（`install_remote` 命令）**

在 `install.rs` 追加（不写单测，靠手工验证——它本质是 ssh/scp 编排）：

```rust
/// 执行安装。返回装好的远端二进制路径。
///
/// 三条路径的共同终点相同：远端出现 `<instance_root>/bin/<version>/yi-agent`
/// 且可执行。任何一步失败都回滚（删掉半成品目录），不留"看起来装好了"的状态。
pub fn install(
    fetcher: &dyn crate::remote::fetch::Fetcher,
    home: &Path,
    instance_root: &str,
    target: &str,
    version: &str,
    caps: &RemoteCaps,
    host: &str,
    from: Option<&Path>,
) -> Result<String, String> {
    let dest = crate::remote::cache::remote_install_dir(instance_root, version);
    let binary = crate::remote::cache::remote_binary_path(instance_root, version);

    match choose_route(caps, false, from) {
        InstallRoute::FromFile => {
            let src = from.expect("route implies a source");
            scp_to_remote(src, host, "/tmp/yi-agent-from")?;
            run_remote(host, &format!(
                "mkdir -p '{dest}' && cp '/tmp/yi-agent-from' '{binary}' && chmod +x '{binary}'"
            ))?;
        }
        InstallRoute::LocalDownload => {
            let cached = crate::remote::cache::cache_path(home, version, target);
            if !cached.exists() {
                crate::remote::fetch::download_verified(fetcher, crate::remote::fetch::RELEASE_BASE, target, version, &cached)?;
            }
            let remote_tmp = format!("/tmp/yi-agent-{version}-{target}.tar.gz");
            scp_to_remote(&cached, host, &remote_tmp)?;
            run_remote(host, &plan(caps, true, None, &remote_tmp, &dest).unpack)?;
            run_remote(host, &format!("chmod +x '{binary}' && rm -f '{remote_tmp}'"))?;
        }
        InstallRoute::RemoteDownload => {
            let url = format!(
                "{}/v{version}/yi-agent-{target}.tar.gz",
                crate::remote::fetch::RELEASE_BASE
            );
            run_remote(host, &format!(
                "mkdir -p '{dest}' && (curl -fsSL '{url}' | tar -xz -C '{dest}' || wget -qO- '{url}' | tar -xz -C '{dest}') && chmod +x '{binary}'"
            ))?;
        }
    }
    Ok(binary)
}

/// 跑一条远端命令，失败时带上 stderr。
fn run_remote(host: &str, command: &str) -> Result<String, String> {
    let out = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=20", host, command])
        .output()
        .map_err(|e| format!("ssh failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "remote command failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn scp_to_remote(local: &Path, host: &str, remote: &str) -> Result<(), String> {
    let out = std::process::Command::new("scp")
        .args([
            "-o", "BatchMode=yes", "-o", "ConnectTimeout=20",
            &local.display().to_string(),
            &format!("{host}:{remote}"),
        ])
        .output()
        .map_err(|e| format!("scp failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "scp failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}
```

- [ ] **Step 6: 跑全部 remote 测试**

```bash
cd desktop/src-tauri && cargo test --lib remote:: 2>&1 | tail -5
```

Expected: 全绿（1+2+3+4+5 任务的测试总和）。

- [ ] **Step 7: 手工端到端验证一次真实安装**

```bash
# 1. 先在远端确认干净
ssh dev@10.0.0.5 'rm -rf /home/dev/.yi-agent-projects/proj-foo'

# 2. 触发安装（临时用 cargo test 里的组装方式，或等计划 3 的设置页）
#    装完后确认：
ssh dev@10.0.0.5 '/home/dev/.yi-agent-projects/proj-foo/.yi-agent/bin/0.1.10/yi-agent --version'

# 3. 确认是可执行文件
ssh dev@10.0.0.5 'file /home/dev/.yi-agent-projects/proj-foo/.yi-agent/bin/0.1.10/yi-agent'
```

Expected: 打印版本号；`file` 显示 ELF 64-bit x86-64。

- [ ] **Step 8: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/remote/install.rs desktop/src-tauri/src/remote/mod.rs
git commit -m "feat(remote): orchestrate install across three routes with fallbacks"
```

---

### Task 6: 缓存清理与版本切换

**Files:**
- Modify: `desktop/src-tauri/src/remote/cache.rs`
- Test: `cache.rs` 内嵌 `mod tests`

**Interfaces:**
- Consumes: `cache_path`（Task 4）
- Produces: `pub fn prune(home: &Path, keep_version: &str) -> std::io::Result<Vec<PathBuf>>`

**为什么单列一个任务：** spec §2.5 说"清理就是删目录"，但没有清理逻辑时缓存会无限增长（每次升级多 15MB）。这是个小任务，但漏了就是磁盘泄漏。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn prune_keeps_the_requested_version_only() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path();
        for v in ["0.1.9", "0.1.10"] {
            let p = cache_path(home, v, "t");
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
        }
        let removed = prune(home, "0.1.10").unwrap();
        assert_eq!(removed.len(), 1, "{removed:?}");
        assert!(cache_path(home, "0.1.10", "t").exists());
        assert!(!cache_path(home, "0.1.9", "t").exists());
    }

    #[test]
    fn prune_on_a_missing_cache_dir_is_a_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(prune(dir.path(), "0.1.10").unwrap().is_empty());
    }
```

- [ ] **Step 2: 实现**

```rust
/// 删掉除 `keep_version` 之外的缓存目录。返回被删的路径。
///
/// 只删 `remote-cache/` 下**看起来像版本号**的目录（首字符是数字），
/// 避免误删用户放进来的别的东西。
pub fn prune(home: &Path, keep_version: &str) -> std::io::Result<Vec<PathBuf>> {
    let root = home.join("remote-cache");
    let mut removed = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(removed),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !path.is_dir() || name == keep_version || !name.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        std::fs::remove_dir_all(&path)?;
        removed.push(path);
    }
    Ok(removed)
}
```

- [ ] **Step 3: 跑测试**

```bash
cd desktop/src-tauri && cargo test --lib remote::cache
```

Expected: 6 passed。

- [ ] **Step 4: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ../..
git add desktop/src-tauri/src/remote/cache.rs
git commit -m "feat(remote): prune stale download caches"
```

---

## 自检（Self-Review）

**1. Spec 覆盖**

| spec 要求 | 对应任务 |
|---|---|
| §2.4.2 路径 1：本机下载 → scp → 远端解压 | Task 5 `LocalDownload` |
| §2.4.2 路径 2：远端直下 | Task 5 `RemoteDownload` |
| §2.4.2 路径 3：`--from` | Task 5 `FromFile` |
| §2.4.2 tar/gzip 能力探测与降级 | Task 3 + Task 5 `unpack_command` |
| §2.4.4 不 scp 本机二进制给异架构 | Task 1（target 决定下载哪个产物） |
| §2.4.5 三 target 支持矩阵 + 显式拒绝 | Task 1 |
| §2.5 按版本分目录 | Task 4 |
| 缓存跨重装存活 | Task 4（在 instance_home 下） |
| sha256 校验 | Task 2 |
| 缓存清理（隐含在 §2.5） | Task 6 |

**2. 占位符扫描**：无 TBD；每个代码步骤都是完整实现。Task 5 的 `install()` 没写单测，原因是它本质是 ssh/scp 编排——已在步骤里明确改用 Step 7 的手工端到端验证，这不是遗漏。

**3. 类型一致性**：
- `RemoteCaps { target, tar_supports_gzip, has_tar }` 在 Task 3 定义，Task 5 使用一致
- `InstallRoute` 三变体在 `choose_route` 与 `install()` 的 match 中一致
- `cache_path(home, version, target)` 在 Task 4 定义、Task 5/6 调用一致
- `remote_binary_path(instance_root, version)` / `remote_install_dir` 在 Task 4 定义、Task 5 调用一致
- `RELEASE_BASE` 在 Task 2 定义、Task 5 使用一致

**依赖顺序**：Task 1 → 3（arch 被 probe 用）、Task 2 独立、Task 4 独立、Task 5 依赖 1–4、Task 6 依赖 4。可并行的是 1/2/4。
