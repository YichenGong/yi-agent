//! `mcp_reload` 的纯逻辑:校验入参、把一次动作合并进 `McpConfig`。
//!
//! 与 IO 分离:本模块不读盘、不探测,只操作内存中的配置,便于逐条单测。

use crate::config::{McpConfig, ServerConfig, Transport};

/// `mcp_reload` 的四个动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadAction {
    Add,
    Enable,
    Disable,
    Remove,
}

impl std::str::FromStr for ReloadAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "add" => Ok(Self::Add),
            "enable" => Ok(Self::Enable),
            "disable" => Ok(Self::Disable),
            "remove" => Ok(Self::Remove),
            other => Err(format!(
                "不支持的 action: {other}；只支持 add / enable / disable / remove"
            )),
        }
    }
}

/// 一次请求:动作 + 目标 server 名 + (仅 add 需要的)新配置。
pub struct ReloadRequest {
    pub action: ReloadAction,
    pub name: String,
    pub spec: Option<ServerConfig>,
    pub overwrite: bool,
}

const MAX_NAME_LEN: usize = 64;

/// server 名规则:小写字母/数字/连字符,不能首尾或连续连字符,<=64 字节。
///
/// 与 `naming.rs` 的清洗规则对齐,避免两个不同的名字被清洗成同一个工具名。
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(format!("name 长度必须在 1..={MAX_NAME_LEN} 字节之间"));
    }
    let b = name.as_bytes();
    let ok_char = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-';
    if !b.iter().all(|c| ok_char(*c)) {
        return Err("name 只能用小写字母、数字与连字符".into());
    }
    if b[0] == b'-' || b[b.len() - 1] == b'-' || name.contains("--") {
        return Err("name 不能以连字符开头/结尾，也不能有连续连字符".into());
    }
    Ok(())
}

/// 校验一份 `ServerConfig`:必填项、header 名、`${VAR}` 引用是否已定义。
pub fn validate_spec(sc: &ServerConfig) -> Result<(), String> {
    let transport = sc.transport().map_err(|e| e.to_string())?;

    for (k, v) in &sc.headers {
        if k.is_empty() || !k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            return Err(format!("header 名不合法: {k:?}（只允许字母/数字/连字符）"));
        }
        check_env_refs(&format!("headers.{k}"), v)?;
    }
    for (k, v) in &sc.env {
        check_env_refs(&format!("env.{k}"), v)?;
    }

    if let Transport::Http { url, .. } = transport
        && !(url.starts_with("http://") || url.starts_with("https://"))
    {
        return Err("http 需要以 http:// 或 https:// 开头的 url".into());
    }
    Ok(())
}

/// `${VAR}` 引用必须已定义。未定义直接拒绝:运行时 `expand_headers` 会静默丢弃
/// 该 header(只 warn),用户最终面对 401 却查不到原因。
fn check_env_refs(field: &str, value: &str) -> Result<(), String> {
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else { break };
        let name = &after[..end];
        if std::env::var(name).is_err() {
            return Err(format!("{field} 引用的环境变量 {name} 未定义"));
        }
        rest = &after[end + 1..];
    }
    Ok(())
}

/// 把一次请求合并进 `cfg`。失败时 `cfg` 保持不变(调用方只在 `Ok` 时写盘)。
pub fn apply(cfg: &mut McpConfig, req: &ReloadRequest) -> Result<(), String> {
    validate_name(&req.name)?;
    match req.action {
        ReloadAction::Add => {
            let spec = req
                .spec
                .as_ref()
                .ok_or_else(|| "add 需要提供 server 配置".to_string())?;
            validate_spec(spec)?;
            if cfg.mcp_servers.contains_key(&req.name) && !req.overwrite {
                return Err(format!(
                    "server {:?} 已存在；要覆盖请传 overwrite: true",
                    req.name
                ));
            }
            cfg.mcp_servers.insert(req.name.clone(), spec.clone());
            Ok(())
        }
        ReloadAction::Enable | ReloadAction::Disable => {
            let on = req.action == ReloadAction::Enable;
            let entry = cfg
                .mcp_servers
                .get_mut(&req.name)
                .ok_or_else(|| format!("没有名为 {:?} 的 server", req.name))?;
            entry.enabled = on;
            Ok(())
        }
        ReloadAction::Remove => {
            if cfg.mcp_servers.remove(&req.name).is_none() {
                return Err(format!("没有名为 {:?} 的 server", req.name));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerKind;
    use std::collections::BTreeMap;

    fn stdio(cmd: &str) -> ServerConfig {
        ServerConfig {
            kind: ServerKind::Stdio,
            command: Some(cmd.into()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
        }
    }

    fn cfg_with(name: &str, sc: ServerConfig) -> McpConfig {
        let mut cfg = McpConfig::default();
        cfg.mcp_servers.insert(name.into(), sc);
        cfg
    }

    #[test]
    fn parses_the_four_actions() {
        assert_eq!("add".parse::<ReloadAction>().unwrap(), ReloadAction::Add);
        assert_eq!(
            "enable".parse::<ReloadAction>().unwrap(),
            ReloadAction::Enable
        );
        assert_eq!(
            "disable".parse::<ReloadAction>().unwrap(),
            ReloadAction::Disable
        );
        assert_eq!(
            "remove".parse::<ReloadAction>().unwrap(),
            ReloadAction::Remove
        );
        let err = "destroy".parse::<ReloadAction>().unwrap_err();
        assert!(err.contains("destroy"), "got {err}");
    }

    #[test]
    fn rejects_invalid_names() {
        for bad in ["GitHub", "-gh", "gh-", "gh--x", "", "a".repeat(65).as_str()] {
            assert!(validate_name(bad).is_err(), "{bad:?} must be rejected");
        }
        for good in ["gh", "github-mcp", "a1"] {
            assert!(validate_name(good).is_ok(), "{good:?} must be accepted");
        }
    }

    #[test]
    fn rejects_stdio_without_command() {
        let mut sc = stdio("");
        sc.command = None;
        let err = validate_spec(&sc).unwrap_err();
        assert!(err.contains("command"), "got {err}");
    }

    #[test]
    fn rejects_http_without_scheme() {
        let sc = ServerConfig {
            kind: ServerKind::Http,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: Some("example.com/mcp".into()),
            headers: BTreeMap::new(),
            enabled: true,
        };
        let err = validate_spec(&sc).unwrap_err();
        assert!(err.contains("http://"), "got {err}");
    }

    #[test]
    fn rejects_undefined_env_reference() {
        let mut sc = stdio("npx");
        sc.env
            .insert("TOKEN".into(), "${MCP_TEST_UNSET_VAR_XYZ}".into());
        let err = validate_spec(&sc).unwrap_err();
        assert!(err.contains("MCP_TEST_UNSET_VAR_XYZ"), "got {err}");
    }

    #[test]
    fn accepts_a_defined_env_reference() {
        // PATH 一定存在;用它做"已定义"的正例。
        let mut sc = stdio("npx");
        sc.env.insert("TOKEN".into(), "${PATH}".into());
        assert!(validate_spec(&sc).is_ok());
    }

    #[test]
    fn rejects_invalid_header_name() {
        let sc = ServerConfig {
            kind: ServerKind::Http,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: Some("https://example.com/mcp".into()),
            headers: BTreeMap::from([("Bad Header".into(), "x".into())]),
            enabled: true,
        };
        let err = validate_spec(&sc).unwrap_err();
        assert!(err.contains("header"), "got {err}");
    }

    #[test]
    fn add_refuses_existing_name_without_overwrite() {
        let mut cfg = cfg_with("gh", stdio("npx"));
        let req = ReloadRequest {
            action: ReloadAction::Add,
            name: "gh".into(),
            spec: Some(stdio("uvx")),
            overwrite: false,
        };
        let err = apply(&mut cfg, &req).unwrap_err();
        assert!(err.contains("已存在"), "got {err}");
        assert_eq!(
            cfg.mcp_servers["gh"].command.as_deref(),
            Some("npx"),
            "a rejected add must not mutate the config"
        );
    }

    #[test]
    fn add_overwrites_when_told_to() {
        let mut cfg = cfg_with("gh", stdio("npx"));
        let req = ReloadRequest {
            action: ReloadAction::Add,
            name: "gh".into(),
            spec: Some(stdio("uvx")),
            overwrite: true,
        };
        apply(&mut cfg, &req).unwrap();
        assert_eq!(cfg.mcp_servers["gh"].command.as_deref(), Some("uvx"));
    }

    #[test]
    fn disable_sets_flag_without_touching_other_fields() {
        let mut sc = stdio("npx");
        sc.args = vec!["-y".into(), "pkg".into()];
        let mut cfg = cfg_with("gh", sc);
        let req = ReloadRequest {
            action: ReloadAction::Disable,
            name: "gh".into(),
            spec: None,
            overwrite: false,
        };
        apply(&mut cfg, &req).unwrap();
        let got = &cfg.mcp_servers["gh"];
        assert!(!got.enabled);
        assert_eq!(got.args, vec!["-y", "pkg"]);
    }

    #[test]
    fn enable_sets_flag_back() {
        let mut sc = stdio("npx");
        sc.enabled = false;
        let mut cfg = cfg_with("gh", sc);
        let req = ReloadRequest {
            action: ReloadAction::Enable,
            name: "gh".into(),
            spec: None,
            overwrite: false,
        };
        apply(&mut cfg, &req).unwrap();
        assert!(cfg.mcp_servers["gh"].enabled);
    }

    #[test]
    fn remove_deletes_the_entry() {
        let mut cfg = cfg_with("gh", stdio("npx"));
        let req = ReloadRequest {
            action: ReloadAction::Remove,
            name: "gh".into(),
            spec: None,
            overwrite: false,
        };
        apply(&mut cfg, &req).unwrap();
        assert!(cfg.mcp_servers.is_empty());
    }

    #[test]
    fn acting_on_an_unknown_server_is_an_error() {
        let mut cfg = McpConfig::default();
        for action in [
            ReloadAction::Enable,
            ReloadAction::Disable,
            ReloadAction::Remove,
        ] {
            let req = ReloadRequest {
                action,
                name: "nope".into(),
                spec: None,
                overwrite: false,
            };
            assert!(
                apply(&mut cfg, &req).is_err(),
                "{action:?} on unknown server"
            );
        }
    }
}
