//! `ProfileKey` 与 profile 字段校验。
//!
//! 字符集约束来自 spec §4.9：`id` 会被用作事件名后缀与目录名末段，
//! `host` / `remote_path` 会被拼进 ssh 命令行。三者都必须是白名单校验，
//! 而不是"看起来没问题就放过去"。

use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
