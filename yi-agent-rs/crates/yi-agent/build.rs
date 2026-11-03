//! 生成构建标识：release / dev / nogit。
//!
//! `Cargo.toml` 的版本永远是下一个正式版的干净号；是否发布由 git 状态判定，
//! 不写进版本字段——发布流水线要求 tag 与 Cargo 版本严格相等。

use std::process::Command;

// The same pure module the integration test compiles, so the id the binary
// embeds and the property the test asserts cannot drift apart.
include!("build_identity.rs");

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let repo_root = Path::new(&manifest_dir).join("../..");

    // 只监听**确实存在**的路径。cargo 把不存在的 rerun-if-changed 路径当作
    // 「永远变化」，于是 build script 每次重跑——行为看着对，实际是巧合。
    //
    // 监听目标的选择：
    // - `refs/tags/`：创建/删除 tag 会改这个目录，而这正是 release/dev 的
    //   判定依据。不监听它，打了 tag 也不会重建，`--version` 继续报 `-dev`。
    // - `HEAD`：切分支/提交会改它。
    // - `index`：暂存区变化会影响 dirty 标记。
    //
    // worktree 里 `.git` 是一个文件而非目录（内容是 `gitdir: <path>`），所以
    // 这里用 `git rev-parse --git-dir` 取真实位置，而不是硬拼 `repo_root/.git`。
    // worktree 有两套 git dir：HEAD/index 在 per-worktree 的 `.git/worktrees/<n>/`，
    // 而 refs（含 tags）在共享的 common dir。取错一个就会监听不存在的路径。
    let per_worktree =
        git(&repo_root, &["rev-parse", "--absolute-git-dir"]).map(std::path::PathBuf::from);
    let common = git(
        &repo_root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map(std::path::PathBuf::from);
    let mut watched: Vec<std::path::PathBuf> = Vec::new();
    if let Some(dir) = common.as_deref() {
        watched.push(dir.join("refs").join("tags"));
    }
    if let Some(dir) = per_worktree.as_deref() {
        watched.push(dir.join("HEAD"));
        watched.push(dir.join("index"));
    }
    let mut declared = false;
    for path in &watched {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
            declared = true;
        }
    }
    if !declared {
        // 没有可监听的 git 信息（如从打包源码构建）：退回「每次重跑」，
        // 宁可多跑一次也不要给出过期版本号。
        println!("cargo:rerun-if-changed=build.rs");
        println!("cargo:warning=no git metadata found; version is derived without it");
    }

    // 发布流水线可显式指定，避免浅克隆下误判。
    println!("cargo:rerun-if-env-changed=YI_AGENT_BUILD_KIND");
    // `Cargo.toml` 的版本号在发布 bump 时会变，必须监听。
    println!(
        "cargo:rerun-if-changed={}",
        Path::new(&manifest_dir).join("Cargo.toml").display()
    );

    let cargo_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let forced = std::env::var("YI_AGENT_BUILD_KIND")
        .ok()
        .filter(|v| !v.is_empty());

    let (version, kind) = match forced.as_deref() {
        Some("release") => (cargo_version, "release"),
        Some(other) => panic!("YI_AGENT_BUILD_KIND must be `release`, got `{other}`"),
        None => {
            let tag = git(&repo_root, &["describe", "--tags", "--exact-match"]);
            match tag {
                Some(tag) if tag == format!("v{cargo_version}") => {
                    (cargo_version.clone(), "release")
                }
                _ => {
                    let sha = git(&repo_root, &["rev-parse", "--short", "HEAD"]);
                    match sha {
                        Some(sha) => {
                            let dirty = git(
                                &repo_root,
                                &["status", "--porcelain", "--untracked-files=no"],
                            )
                            .filter(|s| !s.trim().is_empty())
                            .map(|_| "-dirty")
                            .unwrap_or("");
                            (format!("{cargo_version}-dev+{sha}{dirty}"), "dev")
                        }
                        None => (format!("{cargo_version}+nogit"), "nogit"),
                    }
                }
            }
        }
    };

    println!("cargo:rustc-env=YI_AGENT_VERSION={version}");
    println!("cargo:rustc-env=YI_AGENT_BUILD_KIND={kind}");

    // ---- Daemon artifact identity -------------------------------------------
    //
    // The id must describe the produced *executable*, not the wrapper version and
    // not `git SHA + dirty` (see build_identity.rs for why). It is embedded as a
    // compile-time constant and compared at runtime against the bundled sidecar's
    // manifest.
    let build_id = daemon_build_id(Path::new(&manifest_dir), &repo_root);
    println!("cargo:rustc-env=YI_AGENT_DAEMON_BUILD_ID={build_id}");
}

/// Compute this binary's artifact id from its real build inputs.
fn daemon_build_id(manifest_dir: &Path, repo_root: &Path) -> String {
    // Declare a rerun for every source file we digest. `build.rs` writes these
    // before returning; a new/removed file is caught because cargo re-runs the
    // script when any declared path changes, and the directory itself is declared
    // for additions.
    let mut sources = collect_sources(manifest_dir, &[]);
    for (relative, _) in &sources {
        println!(
            "cargo:rerun-if-changed={}",
            manifest_dir.join(relative).display()
        );
    }
    for dir in ["src", "tests"] {
        let path = manifest_dir.join(dir);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    let mut manifest = std::fs::read(manifest_dir.join("Cargo.toml")).unwrap_or_default();
    // The lockfile pins every dependency's exact version, so a dependency bump at
    // an unchanged SHA must change the id.
    let lock = repo_root.join("Cargo.lock");
    if let Ok(lock_bytes) = std::fs::read(&lock) {
        println!("cargo:rerun-if-changed={}", lock.display());
        manifest.extend_from_slice(&lock_bytes);
    }
    // Workspace-level build configuration.
    for config in [
        repo_root.join("Cargo.toml"),
        repo_root.join(".cargo").join("config.toml"),
    ] {
        if let Ok(bytes) = std::fs::read(&config) {
            println!("cargo:rerun-if-changed={}", config.display());
            manifest.extend_from_slice(&bytes);
        }
    }

    // Only build-relevant environment enters the identity. Secrets are never
    // read, and unrelated variables (PATH, HOME, CI metadata) are ignored so the
    // same source builds to the same id on a different machine.
    let mut rustflags = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    if rustflags.is_empty() {
        rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
    }
    println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_TRIPLE");
    println!("cargo:rerun-if-env-changed=PROFILE");

    let target_triple = std::env::var("CARGO_CFG_TARGET_TRIPLE")
        .or_else(|_| std::env::var("TARGET"))
        .unwrap_or_default();
    let rustc_version =
        Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string()))
            .arg("--version")
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|text| text.trim().to_string())
            .unwrap_or_default();

    let features = features_from_env();

    // Sort for determinism; the digest also sorts, but keeping the input tidy
    // makes the report readable.
    sources.sort_by(|a, b| a.0.cmp(&b.0));

    build_id(
        BuildInputs {
            sources,
            target_triple,
            rustc_version,
            profile: std::env::var("PROFILE").unwrap_or_default(),
            features,
            rustflags,
            manifest,
        }
        .into(),
    )
}

/// `CARGO_FEATURE_<NAME>` is set for every enabled feature.
fn features_from_env() -> Vec<String> {
    let mut features: Vec<String> = std::env::vars()
        .filter_map(|(key, _)| {
            let name = key.strip_prefix("CARGO_FEATURE_")?;
            Some(name.to_ascii_lowercase().replace('_', "-"))
        })
        .collect();
    features.sort();
    features.dedup();
    features
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}
