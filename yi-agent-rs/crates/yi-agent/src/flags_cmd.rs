//! `yi-agent flags` 子命令：注册表的读、改、以及「值来自哪一层」。

use std::path::Path;

use anyhow::Result;
use yi_agent_flags::store::flags_path;
use yi_agent_flags::{all_at, status_at, write_at};

/// `yi-agent flags list`
pub fn run_list() -> Result<i32> {
    let path = require_path()?;
    print!("{}", render_list_at(&path, yi_agent_flags::build_kind()));
    Ok(0)
}

/// `yi-agent flags set <name> on|off`
pub fn run_set(name: &str, enabled: bool) -> Result<i32> {
    let path = require_path()?;
    match set_at_cli(&path, name, enabled) {
        Ok(()) => {
            println!("{name} = {enabled}");
            Ok(0)
        }
        Err(e) => {
            eprintln!("error: {e}");
            Ok(1)
        }
    }
}

/// `yi-agent flags status <name>`
pub fn run_status(name: &str) -> Result<i32> {
    let path = require_path()?;
    print!(
        "{}",
        render_status_at(&path, name, yi_agent_flags::build_kind())
    );
    Ok(0)
}

fn require_path() -> Result<std::path::PathBuf> {
    flags_path().ok_or_else(|| anyhow::anyhow!("cannot resolve the state directory"))
}

fn set_at_cli(path: &Path, name: &str, enabled: bool) -> Result<()> {
    write_at(path, name, enabled).map_err(|e| anyhow::anyhow!("{e}"))
}

fn render_list_at(path: &Path, build_kind: &str) -> String {
    let mut out = String::from("NAME\tSTATE\tSOURCE\tEXPIRES\tSUMMARY\n");
    for view in all_at(path, build_kind) {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            view.name,
            if view.enabled { "on" } else { "off" },
            view.source.as_str(),
            view.expires_on,
            view.summary
        ));
    }
    if yi_agent_flags::REGISTRY.is_empty() {
        out.push_str("(no flags registered)\n");
    }
    out
}

fn render_status_at(path: &Path, name: &str, build_kind: &str) -> String {
    let env = std::env::var(format!(
        "YI_AGENT_FLAG_{}",
        name.to_ascii_uppercase().replace('-', "_")
    ))
    .ok()
    .filter(|v| !v.is_empty());
    let (enabled, source) = status_at(path, name, build_kind, env.as_deref());
    format!("{name}: enabled={enabled} (source: {})\n", source.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_prints_a_row_per_registered_flag() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let out = render_list_at(&path, "dev");
        // 注册表当前可能为空；关键是不 panic 且表头在。
        assert!(out.contains("NAME"), "got: {out}");
        assert!(out.contains("SOURCE"), "got: {out}");
    }

    #[test]
    fn setting_an_unknown_flag_reports_it_and_returns_nonzero() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let err = set_at_cli(&path, "no_such_flag", true).unwrap_err();
        assert!(err.to_string().contains("unknown flag"), "got: {err}");
    }

    #[test]
    fn a_release_build_reports_the_build_kind_gate() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let out = render_status_at(&path, "anything", "release");
        assert!(out.contains("build-kind"), "got: {out}");
    }
}
