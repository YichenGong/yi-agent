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
            ProfileError::BadId(v) => {
                write!(f, "profile id `{v}` must match ^[a-z0-9][a-z0-9-]{{0,31}}$")
            }
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

/// 由（本机 relay URL, profile id）派生远端要连的 session。
///
/// **不能用 `.push_str("&session=")` 追加**：`relay_url` 本身可能已经带
/// `?session=`，追加会得到 `?session=a&session=b`，中继取哪个取决于实现
/// （spec §4.6 前提 2）。这里模拟 URL 的 `searchParams.set` 语义。
///
/// 派生规则：`<原有 session 或 "remote">-<profile_id>`。目的只是「不同 profile
/// 得到不同 session」，不需要密码学强度——但**必须**稳定，否则同一项目每次
/// 重连都会在手机上多出一个 session。
pub fn derive_relay(configured: Option<&str>, profile_id: &str) -> Option<String> {
    let base = configured?;
    // **幂等**：已经带着本项目后缀的 session 原样返回。与 app-server 的
    // `remote_session::derive_session` 逐字同构（见该文件顶部的说明与两处
    // `SHARED_CASES`）。稳定是为了不每次重连在手机上多出一个 session。
    let seed = session_seed(base);
    let suffix = format!("-{profile_id}");
    if seed.ends_with(&suffix) {
        return Some(base.to_string());
    }
    Some(set_query_param(base, "session", &format!("{seed}{suffix}")))
}

fn session_seed(url: &str) -> String {
    get_query_param(url, "session").unwrap_or_else(|| "remote".to_string())
}

fn get_query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query
        .split('&')
        .find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| v.to_string())
        })
        .filter(|v| !v.is_empty())
}

fn set_query_param(url: &str, key: &str, value: &str) -> String {
    let (head, query) = match url.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (url, None),
    };
    let mut pairs: Vec<String> = query
        .map(|query| {
            query
                .split('&')
                .filter(|pair| !pair.is_empty() && !pair.starts_with(&format!("{key}=")))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    pairs.push(format!("{key}={value}"));
    format!("{head}?{}", pairs.join("&"))
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
            assert!(
                validate_profile_id(bad).is_err(),
                "`{bad}` should be rejected"
            );
        }
    }

    /// 既有 session 参数必须被**替换**，不是追加。
    ///
    /// 断言解析后的参数对，而不是对整串做子串匹配：派生值本身以旧 session
    /// 为前缀（`old-proj-foo`），子串匹配会把正确的输出误判成"还留着旧的"。
    #[test]
    fn derive_relay_replaces_an_existing_session_param_exactly_once() {
        let out = derive_relay(Some("wss://r/?session=old"), "proj-foo").unwrap();
        let (_, query) = out.split_once('?').expect("query present");
        let sessions: Vec<&str> = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .filter(|(k, _)| *k == "session")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(sessions.len(), 1, "exactly one session param: {out}");
        assert_eq!(sessions[0], "old-proj-foo", "replaced, not appended: {out}");
    }

    #[test]
    fn derive_relay_appends_a_query_when_there_is_none() {
        let out = derive_relay(Some("wss://r/connect"), "p").unwrap();
        assert!(out.starts_with("wss://r/connect?session="), "{out}");
    }

    #[test]
    fn derive_relay_keeps_unrelated_query_params() {
        let out = derive_relay(Some("wss://r/?token=t&session=s"), "p").unwrap();
        assert!(out.contains("token=t"), "{out}");
        assert_eq!(out.matches("session=").count(), 1, "{out}");
    }

    #[test]
    fn derive_relay_is_stable_for_the_same_input() {
        let a = derive_relay(Some("wss://r/?session=s"), "p").unwrap();
        let b = derive_relay(Some("wss://r/?session=s"), "p").unwrap();
        assert_eq!(
            a, b,
            "an unstable session would leak a phone session per reconnect"
        );
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

    /// 与 app-server 的 `remote_session::SHARED_CASES` **逐字相同**的用例表。
    ///
    /// app-server 不能依赖本 crate，故 `derive_relay` 与
    /// `remote_session::derive_session` 是两份实现。两边派生出不同的 session
    /// 时，手机拿到的地址没有一个进程在监听——而两端各自的日志都正常。这条
    /// 测试是唯一能钉住这个跨 crate 约定的手段。
    const SHARED_CASES: &[(&str, &str, &str)] = &[
        (
            "wss://r/?session=base",
            "proj-foo",
            "wss://r/?session=base-proj-foo",
        ),
        ("wss://r/connect", "p", "wss://r/connect?session=remote-p"),
        (
            "wss://r/?token=t&session=s",
            "p",
            "wss://r/?token=t&session=s-p",
        ),
        (
            "wss://r/?session=old",
            "proj-foo",
            "wss://r/?session=old-proj-foo",
        ),
        ("wss://r/?token=t", "p", "wss://r/?token=t&session=remote-p"),
        (
            "ws://127.0.0.1:8080/ws",
            "a",
            "ws://127.0.0.1:8080/ws?session=remote-a",
        ),
        // 幂等：远端 app-server 对自己已派生过的 session 再派生一次必须是 no-op。
        (
            "wss://r/?session=base-proj-foo",
            "proj-foo",
            "wss://r/?session=base-proj-foo",
        ),
    ];

    #[test]
    fn derive_relay_matches_the_shared_table_verbatim() {
        for (url, id, expected) in SHARED_CASES {
            assert_eq!(
                derive_relay(Some(url), id).as_deref(),
                Some(*expected),
                "case {url} / {id}"
            );
        }
    }

    #[test]
    fn deriving_twice_for_the_same_profile_is_a_noop() {
        let once = derive_relay(Some("wss://r/?session=base"), "proj-foo").unwrap();
        assert_eq!(
            derive_relay(Some(&once), "proj-foo").as_deref(),
            Some(&*once)
        );
    }

    #[test]
    fn derivation_still_differs_for_another_profile() {
        let once = derive_relay(Some("wss://r/?session=base"), "proj-foo").unwrap();
        let other = derive_relay(Some(&once), "proj-bar").unwrap();
        assert_ne!(other, once);
        assert!(other.ends_with("session=base-proj-foo-proj-bar"), "{other}");
    }

    #[test]
    fn an_empty_session_falls_back_to_the_default_seed() {
        assert_eq!(
            derive_relay(Some("wss://r/?session="), "p").as_deref(),
            Some("wss://r/?session=remote-p")
        );
    }

    #[test]
    fn profile_id_rejects_more_than_32_chars() {
        let long = "a".repeat(33);
        assert!(validate_profile_id(&long).is_err());
        let exactly = "a".repeat(32);
        assert!(validate_profile_id(&exactly).is_ok());
    }
}
