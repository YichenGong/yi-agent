//! 生成构建标识：release / dev / nogit。
//!
//! `Cargo.toml` 的版本永远是下一个正式版的干净号；是否发布由 git 状态判定，
//! 不写进版本字段——发布流水线要求 tag 与 Cargo 版本严格相等。

use std::path::Path;
use std::process::Command;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let repo_root = Path::new(&manifest_dir).join("../..");
    let git_dir = repo_root.join(".git");
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    println!("cargo:rerun-if-changed={}", git_dir.join("index").display());
    // 发布流水线可显式指定，避免浅克隆下误判。
    println!("cargo:rerun-if-env-changed=YI_AGENT_BUILD_KIND");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");

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
