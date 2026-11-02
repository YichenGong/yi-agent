//! `mcp_reload` 内置工具:让模型在对话里增删改 MCP server 配置。
//!
//! 一次调用 = 改配置 + 写盘 + (必要时)重新探测 + 回一段给人看的结果。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use yi_agent_core::permission::BlocklistFn;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

use crate::config::{McpConfig, ServerConfig, ServerKind};
use crate::manager::{McpManager, now_unix};
use crate::reload::{ReloadAction, ReloadRequest, apply};

pub const TOOL_NAME: &str = "mcp_reload";

/// 增删改 MCP server 配置。
pub struct McpReloadTool {
    manager: Arc<McpManager>,
    workdir: PathBuf,
    /// 可选命令黑名单;`add` 时用它拒绝明显危险的 command。
    blocklist: Option<BlocklistFn>,
}

impl McpReloadTool {
    pub fn new(manager: Arc<McpManager>, workdir: impl Into<PathBuf>) -> Self {
        Self {
            manager,
            workdir: workdir.into(),
            blocklist: None,
        }
    }

    /// 挂上命令黑名单校验(与 bash 工具共用同一份规则)。
    pub fn with_blocklist(mut self, blocklist: BlocklistFn) -> Self {
        self.blocklist = Some(blocklist);
        self
    }

    /// 校验 `add` 的 command:黑名单命中直接拒绝;含 `/` 的必须是存在的绝对路径。
    ///
    /// 只在配置**写入前**检查:写进去的命令最终会被 spawn,写错时错误会延后到
    /// 探测阶段,用户看到的是一个莫名其妙的超时。
    fn check_command(&self, sc: &ServerConfig) -> Result<(), String> {
        let Some(cmd) = sc.command.as_deref().filter(|c| !c.is_empty()) else {
            return Ok(()); // http 传输或 stdio 缺 command(由 validate_spec 报)
        };
        if let Some(reason) = self.blocklist.as_ref().and_then(|f| f(cmd)) {
            return Err(format!("command {cmd:?} 被安全策略拒绝: {reason}"));
        }
        if cmd.contains('/') {
            let p = Path::new(cmd);
            if !p.is_absolute() {
                return Err(format!("command {cmd:?} 含 '/' 但不是绝对路径"));
            }
            if !p.exists() {
                return Err(format!("command {cmd:?} 不存在"));
            }
        }
        Ok(())
    }

    async fn run(&self, args: Value) -> ToolResult {
        let req = match parse_request(&args) {
            Ok(r) => r,
            Err(e) => return ToolResult::error(e),
        };
        if req.action == ReloadAction::Add
            && let Some(sc) = req.spec.as_ref()
            && let Err(e) = self.check_command(sc)
        {
            return ToolResult::error(e);
        }

        let mut cfg = match McpConfig::load(&self.workdir) {
            Ok(Some(c)) => c,
            Ok(None) => McpConfig::default(),
            Err(e) => return ToolResult::error(format!("读取现有配置失败: {e:#}")),
        };
        if let Err(e) = apply(&mut cfg, &req) {
            return ToolResult::error(e);
        }
        if let Err(e) = cfg.save(&self.workdir) {
            return ToolResult::error(format!("写入配置失败: {e:#}"));
        }

        // enable/disable 同时改内存开关:磁盘值可能与内存里的旧值相同
        // (TUI 的 `/mcp disable` 只动内存),只靠"磁盘变了"检测不到这次
        // 显式意图,`reload` 会继续沿用旧开关,工具仍然不可用。
        if matches!(req.action, ReloadAction::Enable | ReloadAction::Disable) {
            let _ = self
                .manager
                .set_server(&req.name, req.action == ReloadAction::Enable);
        }

        // disable / remove 不需要联网:配置里删掉的工具本轮就不该再出现。
        if matches!(req.action, ReloadAction::Disable | ReloadAction::Remove) {
            return ToolResult::text(render_local(&req, &cfg));
        }

        let report = self.manager.reload(&self.workdir, now_unix()).await;
        ToolResult::text(render_with_report(&req, &report))
    }
}

#[async_trait]
impl Tool for McpReloadTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "enable", "disable", "remove"],
                    "description": "add 新增或覆盖一个 server；enable/disable 切换；remove 删除"
                },
                "name": {
                    "type": "string",
                    "description": "server 名，只能用小写字母、数字与连字符（如 github、horizon-websearch）"
                },
                "kind": {
                    "type": "string",
                    "enum": ["stdio", "http"],
                    "description": "传输方式；stdio 需要 command，http 需要 url"
                },
                "command": {
                    "type": "string",
                    "description": "stdio：启动命令，如 npx / uvx / node"
                },
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "stdio：命令参数，如 [\"-y\", \"@modelcontextprotocol/server-github\"]"
                },
                "env": {
                    "type": "object",
                    "additionalProperties": {"type": "string"},
                    "description": "stdio：环境变量；可用 ${VAR} 引用已存在的环境变量"
                },
                "url": {
                    "type": "string",
                    "description": "http：以 http:// 或 https:// 开头的服务地址"
                },
                "headers": {
                    "type": "object",
                    "additionalProperties": {"type": "string"},
                    "description": "http：请求头；可用 ${VAR} 引用已存在的环境变量"
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "add 时若同名 server 已存在，是否覆盖（默认 false，会报错）"
                }
            },
            "required": ["action", "name"]
        })
    }

    fn description(&self) -> &str {
        "新增、启用、停用或删除一个 MCP server，并立即写盘。返回结果会说明每个 server \
         探测到多少工具，或失败原因。新工具从下一条消息起可用；本轮不要尝试调用它们。"
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Builtin,
            // 改的是用户的持久配置，且会 spawn 探测进程:必须每次确认。
            requires_confirmation: true,
            read_only: false,
            version: None,
        }
    }

    async fn call(&self, args: Value) -> ToolResult {
        self.run(args).await
    }
}

fn parse_request(args: &Value) -> Result<ReloadRequest, String> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| "缺少 action".to_string())?
        .parse::<ReloadAction>()?;
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "缺少 name".to_string())?
        .to_string();

    let spec = if action == ReloadAction::Add {
        Some(parse_spec(args)?)
    } else {
        None
    };

    Ok(ReloadRequest {
        action,
        name,
        spec,
        overwrite: args
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn parse_spec(args: &Value) -> Result<ServerConfig, String> {
    let kind = match args.get("kind").and_then(Value::as_str).unwrap_or("stdio") {
        "stdio" => ServerKind::Stdio,
        "http" => ServerKind::Http,
        other => return Err(format!("kind 只支持 stdio / http，收到 {other:?}")),
    };
    Ok(ServerConfig {
        kind,
        command: args
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string),
        args: args
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        env: str_map(args.get("env")),
        url: args.get("url").and_then(Value::as_str).map(str::to_string),
        headers: str_map(args.get("headers")),
        enabled: true,
    })
}

fn str_map(v: Option<&Value>) -> BTreeMap<String, String> {
    v.and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// `disable` / `remove`:不探测,直接说清楚改了什么、什么时候生效。
fn render_local(req: &ReloadRequest, cfg: &McpConfig) -> String {
    let verb = match req.action {
        ReloadAction::Disable => "已停用",
        ReloadAction::Remove => "已删除",
        _ => "已更新",
    };
    format!(
        "{verb} MCP server `{}`。\n配置已写入 `.yi-agent/mcp.json`；相关工具从下一条消息起不再可用，本轮不要再调用它们。\n当前共 {} 个 server。",
        req.name,
        cfg.mcp_servers.len()
    )
}

fn render_with_report(req: &ReloadRequest, report: &crate::manager::ReloadReport) -> String {
    let mut out = String::new();
    let action_line = if req.action == ReloadAction::Enable {
        "已启用".to_string()
    } else if req.spec.is_some() {
        format!("已写入 MCP server `{}`", req.name)
    } else {
        format!("已更新 MCP server `{}`", req.name)
    };
    out.push_str(&action_line);
    out.push_str("，配置已保存到 `.yi-agent/mcp.json`。\n\n探测结果：\n");

    for s in &report.servers {
        if let Some(err) = &s.error {
            out.push_str(&format!("- `{}` 失败：{}\n", s.name, err));
        } else if s.removed {
            out.push_str(&format!("- `{}` 已移除\n", s.name));
        } else if s.probed {
            out.push_str(&format!(
                "- `{}` ✓ {} 个工具（{} 只读）\n",
                s.name, s.tool_count, s.read_only_count
            ));
        } else if s.skipped_backoff {
            // 用户敲 reload 想强制刷新,却被上一次失败的退避窗口挡住。必须说明,
            // 否则他分不清「真没问题」与「被退避挡住」。
            out.push_str(&format!(
                "- `{}` 跳过探测（上次失败后的退避窗口内，最早可重试约 {} 分钟后），沿用 {} 个工具（{} 只读）\n",
                s.name,
                crate::cache::RETRY_AFTER_SECS / 60,
                s.tool_count,
                s.read_only_count
            ));
        } else {
            out.push_str(&format!(
                "- `{}` ✓ {} 个工具（{} 只读，配置未变，未重新探测）\n",
                s.name, s.tool_count, s.read_only_count
            ));
        }
    }

    out.push_str("\n新工具从下一条消息起可用；本轮不要尝试调用它们。");
    if !report.all_ok() {
        out.push_str("\n\n有 server 探测失败。配置本身已经保存；可以修正参数后再次调用 mcp_reload，或让用户检查网络与凭据。");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerKind;
    use crate::manager::now_unix;
    use crate::reload::{ReloadAction, ReloadRequest};
    use tempfile::TempDir;

    fn empty_manager() -> Arc<McpManager> {
        McpManager::empty()
    }

    fn dummy_spec() -> ServerConfig {
        ServerConfig {
            kind: ServerKind::Stdio,
            command: Some("npx".into()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
        }
    }

    fn write_cfg(dir: &Path, json: &str) {
        std::fs::create_dir_all(dir.join(".yi-agent")).unwrap();
        std::fs::write(dir.join(".yi-agent/mcp.json"), json).unwrap();
    }

    fn read_cfg(dir: &Path) -> McpConfig {
        McpConfig::load(dir).unwrap().unwrap()
    }

    fn text_of(r: &ToolResult) -> String {
        r.content
            .iter()
            .filter_map(|b| match b {
                yi_agent_core::ContentBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    #[tokio::test]
    async fn disable_writes_the_flag_and_answers_immediately() {
        let dir = TempDir::new().unwrap();
        write_cfg(
            dir.path(),
            r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","pkg"]}}}"#,
        );
        let tool = McpReloadTool::new(empty_manager(), dir.path());

        let res = tool.call(json!({"action": "disable", "name": "fs"})).await;
        assert!(!res.is_error, "got {}", text_of(&res));

        let cfg = read_cfg(dir.path());
        assert!(!cfg.mcp_servers["fs"].enabled, "flag must be persisted");
        assert_eq!(cfg.mcp_servers["fs"].args, vec!["-y", "pkg"]);

        let text = text_of(&res);
        assert!(text.contains("已停用"), "got {text}");
        assert!(
            text.contains("下一条消息"),
            "must state when it takes effect: {text}"
        );
    }

    /// TUI 的 `/mcp disable` 只动内存(磁盘仍 `enabled: true`),之后
    /// `mcp_reload enable` 写盘的值与磁盘旧值相同——`reload` 的"磁盘变了"
    /// 检测不到这次显式意图,必须由本工具同步内存开关。
    #[tokio::test]
    async fn enable_after_a_runtime_only_disable_takes_effect() {
        let dir = TempDir::new().unwrap();
        write_cfg(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let spec = crate::config::ServerConfig {
            kind: crate::config::ServerKind::Stdio,
            command: Some("npx".into()),
            ..dummy_spec()
        };
        // 预置缓存,避免 reload 真的 spawn。
        let mut cache = crate::test_support::cache_with(dir.path());
        cache.put(
            "fs",
            &spec,
            vec![crate::test_support::cached_tool("read", true)],
            now_unix(),
        );
        cache.save(dir.path()).unwrap();

        let manager = McpManager::empty();
        manager.reload(dir.path(), now_unix()).await;
        manager.set_server("fs", false).unwrap();
        assert!(!manager.is_effective("fs"), "runtime disable must apply");

        let tool = McpReloadTool::new(Arc::clone(&manager), dir.path());
        let res = tool.call(json!({"action": "enable", "name": "fs"})).await;
        assert!(!res.is_error, "got {}", text_of(&res));
        assert!(
            manager.is_effective("fs"),
            "enable must take effect even though the on-disk value did not change"
        );
    }

    #[tokio::test]
    async fn remove_deletes_the_entry_from_disk() {
        let dir = TempDir::new().unwrap();
        write_cfg(
            dir.path(),
            r#"{"mcpServers":{"fs":{"command":"npx"},"gh":{"command":"uvx"}}}"#,
        );
        let tool = McpReloadTool::new(empty_manager(), dir.path());

        let res = tool.call(json!({"action": "remove", "name": "fs"})).await;
        assert!(!res.is_error, "got {}", text_of(&res));

        let cfg = read_cfg(dir.path());
        assert!(!cfg.mcp_servers.contains_key("fs"));
        assert!(cfg.mcp_servers.contains_key("gh"), "others must survive");
    }

    /// `add` 的配置落盘。用预置缓存避免真的 spawn 一个 `npx`。
    #[tokio::test]
    async fn add_on_a_fresh_project_creates_the_config_file() {
        let dir = TempDir::new().unwrap();
        let spec = ServerConfig {
            kind: ServerKind::Stdio,
            command: Some("npx".into()),
            args: vec!["-y".into(), "pkg".into()],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
        };
        // 缓存命中 ⇒ reload 不再探测,测试不必联网也不必 spawn。
        let mut cache = crate::test_support::cache_with(dir.path());
        cache.put(
            "fs",
            &spec,
            vec![crate::test_support::cached_tool("read", true)],
            now_unix(),
        );
        cache.save(dir.path()).unwrap();

        let tool = McpReloadTool::new(empty_manager(), dir.path());
        let res = tool
            .call(json!({
                "action": "add",
                "name": "fs",
                "kind": "stdio",
                "command": "npx",
                "args": ["-y", "pkg"]
            }))
            .await;
        let text = text_of(&res);
        assert!(!res.is_error, "got {text}");

        let cfg = read_cfg(dir.path());
        assert_eq!(cfg.mcp_servers["fs"].command.as_deref(), Some("npx"));
        assert!(cfg.enabled, "master switch must stay on");
        assert!(
            text.contains("1 个工具"),
            "must show the probe result: {text}"
        );
        assert!(
            text.contains("下一条消息"),
            "must state when it takes effect: {text}"
        );
    }

    /// 探测失败必须原样带回,不能静默成功。
    ///
    /// 直接测渲染:真实探测的失败路径已由 `manager::reload` 的测试覆盖,
    /// 这里关心的是「失败原因有没有出现在给模型看的文本里」。
    #[test]
    fn probe_failure_is_surfaced_not_swallowed() {
        let req = ReloadRequest {
            action: ReloadAction::Add,
            name: "broken".into(),
            spec: Some(dummy_spec()),
            overwrite: false,
        };
        let report = crate::manager::ReloadReport {
            servers: vec![crate::manager::ServerOutcome {
                name: "broken".into(),
                added: true,
                removed: false,
                probed: true,
                skipped_backoff: false,
                tool_count: 0,
                read_only_count: 0,
                error: Some("npx 下载超时".into()),
            }],
        };
        let text = render_with_report(&req, &report);
        assert!(
            text.contains("失败：npx 下载超时"),
            "raw reason must survive: {text}"
        );
        assert!(
            text.contains("下一条消息"),
            "must state when it takes effect: {text}"
        );
        assert!(
            text.contains("有 server 探测失败"),
            "must flag the failure: {text}"
        );
        assert!(!report.all_ok());
    }

    /// 未探测(缓存命中)的 server 也要报出工具数,并说明没重新探测。
    #[test]
    fn rendering_distinguishes_probed_from_cached() {
        let req = ReloadRequest {
            action: ReloadAction::Add,
            name: "fs".into(),
            spec: Some(dummy_spec()),
            overwrite: false,
        };
        let report = crate::manager::ReloadReport {
            servers: vec![crate::manager::ServerOutcome {
                name: "fs".into(),
                added: true,
                removed: false,
                probed: false,
                skipped_backoff: false,
                tool_count: 3,
                read_only_count: 1,
                error: None,
            }],
        };
        let text = render_with_report(&req, &report);
        assert!(text.contains("3 个工具（1 只读"), "got {text}");
        assert!(text.contains("未重新探测"), "got {text}");
        assert!(report.all_ok());
    }

    #[tokio::test]
    async fn rejects_a_blocklisted_command() {
        let dir = TempDir::new().unwrap();
        let blocklist: BlocklistFn =
            Arc::new(|cmd: &str| (cmd == "rm").then(|| "rm is blocked".to_string()));
        let tool = McpReloadTool::new(empty_manager(), dir.path()).with_blocklist(blocklist);

        let res = tool
            .call(json!({"action": "add", "name": "bad", "command": "rm"}))
            .await;
        assert!(res.is_error, "blocklisted command must be rejected");
        assert!(
            McpConfig::load(dir.path()).unwrap().is_none(),
            "nothing must be written"
        );
    }

    #[tokio::test]
    async fn rejects_overwriting_without_the_flag() {
        let dir = TempDir::new().unwrap();
        write_cfg(dir.path(), r#"{"mcpServers":{"fs":{"command":"npx"}}}"#);
        let tool = McpReloadTool::new(empty_manager(), dir.path());

        let res = tool
            .call(json!({"action": "add", "name": "fs", "command": "uvx"}))
            .await;
        assert!(res.is_error);
        assert_eq!(
            read_cfg(dir.path()).mcp_servers["fs"].command.as_deref(),
            Some("npx")
        );
    }

    #[tokio::test]
    async fn rejects_unknown_action_and_missing_name() {
        let dir = TempDir::new().unwrap();
        let tool = McpReloadTool::new(empty_manager(), dir.path());
        assert!(
            tool.call(json!({"action": "nuke", "name": "x"}))
                .await
                .is_error
        );
        assert!(tool.call(json!({"action": "disable"})).await.is_error);
    }

    #[test]
    fn metadata_requires_confirmation_and_is_not_read_only() {
        let dir = TempDir::new().unwrap();
        let tool = McpReloadTool::new(empty_manager(), dir.path());
        let md = tool.metadata();
        assert!(md.requires_confirmation);
        assert!(!md.read_only);
        assert_eq!(md.source, ToolSource::Builtin);
        assert_eq!(tool.name(), "mcp_reload");
    }

    /// 因退避跳过时,报告必须写明原因,不能与「配置未变」混为一谈。
    #[test]
    fn backoff_skip_is_explained_in_the_report() {
        let req = ReloadRequest {
            action: ReloadAction::Enable,
            name: "fs".into(),
            spec: None,
            overwrite: false,
        };
        let report = crate::manager::ReloadReport {
            servers: vec![crate::manager::ServerOutcome {
                name: "fs".into(),
                added: false,
                removed: false,
                probed: false,
                skipped_backoff: true,
                tool_count: 2,
                read_only_count: 1,
                error: None,
            }],
        };
        let text = render_with_report(&req, &report);
        assert!(
            text.contains("跳过") || text.contains("退避"),
            "必须说明为什么没探测, 实际: {text}"
        );
        assert!(
            text.contains("2 个工具"),
            "必须说明沿用了多少个工具, 实际: {text}"
        );
        assert!(
            !text.contains("配置未变"),
            "退避与「配置未变」必须可区分, 实际: {text}"
        );
    }

    /// 汇总方法要能列出因退避跳过的 server,且按名字稳定排序。
    #[test]
    fn skipped_backoff_names_lists_only_backed_off_servers() {
        let mk = |name: &str, skipped: bool| crate::manager::ServerOutcome {
            name: name.into(),
            added: false,
            removed: false,
            probed: false,
            skipped_backoff: skipped,
            tool_count: 0,
            read_only_count: 0,
            error: None,
        };
        let report = crate::manager::ReloadReport {
            servers: vec![mk("zeta", true), mk("alpha", true), mk("mid", false)],
        };
        assert_eq!(report.skipped_backoff_names(), vec!["alpha", "zeta"]);
        let empty = crate::manager::ReloadReport { servers: vec![] };
        assert!(empty.skipped_backoff_names().is_empty());
    }
}
