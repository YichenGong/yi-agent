//! `remote/*` 的 Tauri 命令所依赖的**纯逻辑**：闸门与参数校验。
//!
//! **为什么这些不放 app-server**（spec §3.2）：app-server 是被 ssh 运送的那一端，
//! 把「怎么连远端」放进去等于让远端进程决定怎么连自己。而且开关关闭时远端可能
//! 跑着旧二进制，根本不认识这些方法。
//!
//! 命令本体在 `bridge.rs`（它们需要 `AppHandle` 与槽位表）；本模块只收那些
//! 不碰全局状态、可以穷举测试的判定。

use serde_json::Value;

use super::profile::{validate_profile_id, ProfileError, RemoteProfile};

/// 支持的远端 target 只有这三个（spec §2.4.5）。
///
/// 其余一律**显式拒绝**，不做「大概能跑」的猜测：二进制是按 target 编的，
/// 装错架构的后果是 `Exec format error`，而那个错误发生在远端、用户看不到。
pub const SUPPORTED_ARCHES: [&str; 3] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
];

/// 闸门：开关开着**且**构建是 dev（spec §5.3.1）。
///
/// 抽成纯函数才能测——真实调用里的两个输入都来自进程全局状态，而且测试二进制的
/// `build_kind()` 恒为 `release`，直接调它无法覆盖「开」的那条分支。
pub fn gate(flag_on: bool, build_kind: &str) -> bool {
    flag_on && build_kind == "dev"
}

/// 校验前端传来的 profile。
///
/// 先做**所有**检查再返回第一个错误：逐个来回报错会让设置页反复弹不同的问题，
/// 用户改一个发现还有下一个。
pub fn validate_profile_payload(raw: &Value) -> Result<RemoteProfile, String> {
    let get = |key: &str| {
        raw.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };

    let id = get("id");
    validate_profile_id(&id).map_err(|error| error.to_string())?;

    let arch = get("remote_arch");
    if !SUPPORTED_ARCHES.contains(&arch.as_str()) {
        return Err(format!(
            "remote_arch `{arch}` has no prebuilt yi-agent binary; supported: {}",
            SUPPORTED_ARCHES.join(", ")
        ));
    }

    let host = get("host");
    // 走同一条 `remote_argv` 校验路径，避免这里放过一个会被拼进 ssh 命令行的
    // 值——那是最危险的一类输入。
    check_host(&host)?;

    let instance_root = get("instance_root");
    check_remote_path("instance_root", &instance_root)?;
    let remote_path = get("remote_path");
    check_remote_path("remote_path", &remote_path)?;

    let local_path = get("local_path");
    let path = std::path::Path::new(&local_path);
    if local_path.is_empty() || !path.is_absolute() {
        return Err(format!("local_path `{local_path}` must be absolute"));
    }

    let binary_version = get("binary_version");
    if binary_version.is_empty() {
        return Err("binary_version must not be empty".to_string());
    }

    let label = {
        let raw = get("label");
        if raw.trim().is_empty() {
            id.clone()
        } else {
            raw
        }
    };

    Ok(RemoteProfile {
        id,
        label,
        host,
        local_path: path.to_path_buf(),
        remote_path,
        instance_root,
        binary_version,
        remote_arch: arch,
    })
}

/// `host` 会被拼进 ssh 命令行，必须与 `remote_argv` 用**同一个**判据。
fn check_host(host: &str) -> Result<(), String> {
    if host.trim().is_empty() {
        return Err("host must not be empty".to_string());
    }
    // 复用 argv 的构造来校验：它已经做了字符白名单检查，且这是唯一会被拼进
    // shell 行的那份实现——两处各写一份判据迟早会不一致。
    let probe = RemoteProfile {
        id: "x".to_string(),
        label: "x".to_string(),
        host: host.to_string(),
        local_path: std::path::PathBuf::from("/a"),
        remote_path: "/b".to_string(),
        instance_root: "/c/.yi-agent".to_string(),
        binary_version: "1".to_string(),
        remote_arch: SUPPORTED_ARCHES[2].to_string(),
    };
    match super::argv::remote_argv(&probe, "/c/.yi-agent/bin/yi-agent", None) {
        Ok(_) => Ok(()),
        Err(ProfileError::BadHost(value)) => Err(ProfileError::BadHost(value).to_string()),
        Err(error) => Err(error.to_string()),
    }
}

fn check_remote_path(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if !value.starts_with('/') {
        return Err(format!("{field} `{value}` must be absolute"));
    }
    // 字符集：与 `remote_argv` 的白名单逐字一致（无引号、无 `$`、无分号等）。
    let ok = value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "/._-+@:".contains(ch));
    if !ok {
        return Err(format!(
            "{field} `{value}` may only contain letters, digits and /._-+@:"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload() -> Value {
        json!({
            "id": "proj-foo",
            "label": "foo",
            "host": "dev@host",
            "local_path": "/Users/y/proj/foo",
            "remote_path": "/home/dev/proj/foo",
            "instance_root": "/home/dev/.yi-agent-projects/foo/.yi-agent",
            "binary_version": "0.1.10",
            "remote_arch": "x86_64-unknown-linux-gnu"
        })
    }

    /// 三条闸门乘积：flag 关、非 dev、nogit 都必须关。
    #[test]
    fn remote_commands_are_absent_unless_dev_and_flagged() {
        assert!(!gate(true, "release"));
        assert!(!gate(false, "dev"));
        assert!(gate(true, "dev"));
    }

    /// spec §5.3：`build_kind != "dev"` 一律关。nogit 是「非 git 的 dev 构建」，
    /// 它同样没经过验证流程，不能当 dev 用。
    #[test]
    fn a_nogit_build_is_treated_like_release() {
        assert!(!gate(true, "nogit"));
        assert!(!gate(true, ""));
    }

    #[test]
    fn a_valid_payload_round_trips() {
        let profile = validate_profile_payload(&payload()).unwrap();
        assert_eq!(profile.id, "proj-foo");
        assert_eq!(profile.label, "foo");
        assert_eq!(profile.remote_arch, "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn upsert_rejects_a_bad_profile_id() {
        let mut raw = payload();
        raw["id"] = json!("Bad_ID");
        let error = validate_profile_payload(&raw).unwrap_err();
        assert!(error.contains("Bad_ID"), "{error}");
    }

    #[test]
    fn upsert_rejects_an_unsupported_arch() {
        let mut raw = payload();
        raw["remote_arch"] = json!("aarch64-unknown-linux-gnu");
        let error = validate_profile_payload(&raw).unwrap_err();
        assert!(error.contains("aarch64-unknown-linux-gnu"), "{error}");
        // 报错里必须列出支持清单，否则用户不知道该填什么。
        assert!(error.contains("x86_64-unknown-linux-gnu"), "{error}");
    }

    /// host 会被拼进 ssh 命令行：任何 shell 元字符都必须在这里被拦下。
    #[test]
    fn upsert_rejects_a_host_with_shell_metacharacters() {
        for host in [
            "dev@host; rm -rf /",
            "dev@host$(id)",
            "dev@host`id`",
            "dev@host|x",
        ] {
            let mut raw = payload();
            raw["host"] = json!(host);
            assert!(
                validate_profile_payload(&raw).is_err(),
                "host `{host}` must be rejected"
            );
        }
    }

    #[test]
    fn upsert_rejects_a_relative_local_path() {
        for path in ["", "relative/dir"] {
            let mut raw = payload();
            raw["local_path"] = json!(path);
            let error = validate_profile_payload(&raw).unwrap_err();
            assert!(error.contains("local_path"), "{error}");
        }
    }

    #[test]
    fn upsert_rejects_a_remote_path_that_is_not_absolute() {
        let mut raw = payload();
        raw["remote_path"] = json!("proj/foo");
        let error = validate_profile_payload(&raw).unwrap_err();
        assert!(error.contains("remote_path"), "{error}");
    }

    #[test]
    fn upsert_rejects_a_remote_path_with_shell_metacharacters() {
        let mut raw = payload();
        raw["remote_path"] = json!("/home/dev/$(id)");
        assert!(validate_profile_payload(&raw).is_err());
    }

    #[test]
    fn upsert_rejects_an_empty_binary_version() {
        let mut raw = payload();
        raw["binary_version"] = json!("");
        let error = validate_profile_payload(&raw).unwrap_err();
        assert!(error.contains("binary_version"), "{error}");
    }

    /// 空 label 退回 id：手机上不能出现一张没有标题的卡片。
    #[test]
    fn a_blank_label_falls_back_to_the_id() {
        for label in ["", "   "] {
            let mut raw = payload();
            raw["label"] = json!(label);
            assert_eq!(validate_profile_payload(&raw).unwrap().label, "proj-foo");
        }
    }

    /// 缺字段不能让 `unwrap_or_default` 静默通过所有检查。
    #[test]
    fn a_payload_with_no_fields_is_rejected() {
        assert!(validate_profile_payload(&json!({})).is_err());
    }
}
