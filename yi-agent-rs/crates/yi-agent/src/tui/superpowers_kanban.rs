//! `/superpowers-kanban`——经 daemon 的通用 `plugin/query` 通道与插件对话。
//!
//! 宿主侧不认识看板：这里只拼查询、转发、把插件回的内容渲染成行。开关、卡片、
//! 投递的语义全在插件里，因此插件被卸载后这里只剩一条「插件未安装」的说明。

use std::path::Path;

use serde_json::{Value, json};

/// 看板插件的名字。daemon 只按名字找它清单里声明的 socket。
const KANBAN_PLUGIN: &str = "superpowers-kanban";

/// What `/kanban` produced: lines to show, and whether it changed the switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KanbanOutcome {
    pub lines: Vec<String>,
    /// `Some(true)` / `Some(false)` when this invocation wrote the project layer.
    pub toggled_to: Option<bool>,
}

/// 一次查询的失败原因，决定给用户看哪句话。
enum QueryFailure {
    /// daemon 没在守护这个插件（没装 / 没声明 socket）。
    PluginMissing,
    /// 其他失败：daemon 不可达、插件拒绝、协议错。
    Other(String),
}

/// 经 daemon 问插件一个问题。返回插件的原始应答。
fn query_plugin(workdir: &Path, method: &str, params: Value) -> Result<Value, QueryFailure> {
    let socket = crate::runtime_socket_for(workdir)
        .map_err(|error| QueryFailure::Other(format!("no daemon socket: {error}")))?;
    let response = yi_agent_store::ipc::send_request(
        &socket,
        yi_agent_store::ipc::IpcRequest::PluginQuery {
            plugin: KANBAN_PLUGIN.to_string(),
            method: method.to_string(),
            params,
        },
    )
    .map_err(|error| QueryFailure::Other(format!("daemon is unavailable: {error}")))?;
    match response {
        yi_agent_store::ipc::IpcResponse::PluginResult { value } => Ok(value),
        // The daemon reports "not available" for a plugin it does not supervise;
        // that is the uninstalled case, and it deserves its own wording.
        yi_agent_store::ipc::IpcResponse::Error { message, .. } => {
            let text = message.unwrap_or_default();
            if text.contains("is not available") {
                Err(QueryFailure::PluginMissing)
            } else {
                Err(QueryFailure::Other(text))
            }
        }
        other => Err(QueryFailure::Other(format!(
            "daemon returned an unexpected response: {other:?}"
        ))),
    }
}

/// Handles `/superpowers-kanban [on|off|run|add <spec> <plan>|add-prequeued <title>|promote <id> <spec> <plan> [<title>]]`.
///
/// The board is *disabled by default*, so an accidental invocation cannot start
/// a queue. `run` is intentionally refused while disabled: the switch is the
/// user's explicit consent to let the board act.
///
/// `create`/`remove` are **not** handled here: the host used to drive them
/// itself (writing a registry, launching a daemon), and that host-side board
/// surface is gone. Asking the plugin is the only remaining route, so those two
/// verbs are forwarded over the same generic channel as everything else — see
/// [`handle_kanban_with`].
pub fn handle_kanban(workdir: &Path, args: &str) -> KanbanOutcome {
    let resident_dir = yi_agent_store::resident::default_dir().unwrap_or_default();
    handle_kanban_with(workdir, args, &resident_dir)
}

/// 与 [`handle_kanban`] 相同，但把**通用常驻登记目录**注入进来。
///
/// daemon 对「插件没装」和「本目录压根没建看板」回的话可以一样，只有登记
/// 能把两者分开。所以看板读不出来时要拿它来分辨：未登记的目录要说「用
/// /superpowers-kanban create 创建」，而不是甩一句 `daemon is unavailable`
/// 的连接错误——那句话对用户毫无下一步可言。生产入口 [`handle_kanban`] 传
/// [`yi_agent_store::resident::default_dir`]；注入形参只为让测试不碰真实 `$HOME`。
///
/// 登记**是通用的**：宿主只问「这个项目有没有被任何请求者要求常驻」，不认识
/// 看板。[`yi_agent_store::resident::entries`] 对缺文件/坏文件同样宽容地回空表，
/// 故它不会因为一份坏文件把这里变成一条误导性的 create 指引。
pub fn handle_kanban_with(workdir: &Path, args: &str, resident_dir: &Path) -> KanbanOutcome {
    let argument = args.trim();
    let plain = |lines: Vec<String>| KanbanOutcome {
        lines,
        toggled_to: None,
    };
    let unavailable = || {
        plain(vec![
            "Superpowers 看板插件未安装。看板由插件提供，装上它这里才有卡片。".to_string(),
        ])
    };
    // 看板读不出来（daemon 不可达、插件拒绝、协议错）时的说法：登记里有没有这个
    // 项目，决定是「真故障」还是「还没建」。
    let unreadable = |message: String| {
        let registered = yi_agent_store::resident::list(resident_dir)
            .iter()
            .any(|project| project == workdir);
        plain(vec![if registered {
            format!("无法读取看板: {message}")
        } else {
            "本目录尚未创建看板；用 /superpowers-kanban create 创建。".to_string()
        }])
    };

    match argument {
        "" => match query_plugin(workdir, "list", json!({})) {
            Ok(value) => match render_board(workdir, &value) {
                Ok(lines) => plain(lines),
                Err(message) => plain(vec![message]),
            },
            Err(QueryFailure::PluginMissing) => unavailable(),
            Err(QueryFailure::Other(message)) => unreadable(message),
        },
        // 创建/移除是**插件自己的**动作（它按通用契约登记常驻、建/删自己的
        // 状态目录）。宿主已不再有看板登记表，故这两条经通用通道转给插件，
        // 与 on/off/run/add 走的是同一条路。
        "create" | "remove" => {
            let method = if argument == "create" {
                "lifecycle/create"
            } else {
                "lifecycle/remove"
            };
            match query_plugin(workdir, method, json!({ "project": workdir.to_string_lossy() })) {
                Ok(_) => plain(vec![if argument == "create" {
                    "Superpowers 看板: 已创建（插件已登记本目录的常驻需求）".to_string()
                } else {
                    "Superpowers 看板: 已移除（插件已释放本目录的常驻需求）".to_string()
                }]),
                Err(QueryFailure::PluginMissing) => unavailable(),
                Err(QueryFailure::Other(message)) => {
                    plain(vec![format!("Superpowers 看板 {argument} 失败: {message}")])
                }
            }
        }
        "status" => {
            let registered = yi_agent_store::resident::list(resident_dir)
                .iter()
                .any(|project| project == workdir);
            plain(vec![if registered {
                "本目录的看板已登记；插件进程由常驻值守维护。".to_string()
            } else {
                "本目录尚未创建看板；用 /superpowers-kanban create 创建。".to_string()
            }])
        }
        "on" | "off" => {
            let on = argument == "on";
            match query_plugin(workdir, "switch.write", json!({ "on": on })) {
                Ok(_) => KanbanOutcome {
                    lines: vec![format!(
                        "Superpowers 看板 {}",
                        if on { "enabled" } else { "disabled" }
                    )],
                    toggled_to: Some(on),
                },
                Err(QueryFailure::PluginMissing) => unavailable(),
                Err(QueryFailure::Other(message)) => {
                    plain(vec![format!("无法写入开关: {message}")])
                }
            }
        }
        "run" => match query_plugin(workdir, "switch.read", json!({})) {
            Ok(value) => {
                let on = value.get("on").and_then(Value::as_bool).unwrap_or(false);
                let source = value
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("default");
                if on {
                    plain(vec![
                        "Superpowers 看板 is enabled; the plugin process advances the queue."
                            .to_string(),
                    ])
                } else {
                    plain(vec![format!(
                        "Superpowers 看板 is disabled (source: {source}). \
                         Enable it with /superpowers-kanban on, or set \"superpowers_kanban\": true."
                    )])
                }
            }
            Err(QueryFailure::PluginMissing) => unavailable(),
            Err(QueryFailure::Other(message)) => plain(vec![format!("无法读取开关: {message}")]),
        },
        _ if argument == "add" || argument.starts_with("add ") => {
            let mut parts = argument.split_whitespace();
            let _verb = parts.next();
            match (parts.next(), parts.next(), parts.next()) {
                (Some(spec), Some(plan), None) => {
                    match query_plugin(
                        workdir,
                        "enqueue",
                        json!({ "spec_path": spec, "plan_path": plan }),
                    ) {
                        Ok(value) => {
                            let id = value.get("id").and_then(Value::as_str).unwrap_or("card");
                            plain(vec![format!("Superpowers 看板: delivered {id} to inbox")])
                        }
                        Err(QueryFailure::PluginMissing) => unavailable(),
                        Err(QueryFailure::Other(message)) => plain(vec![format!(
                            "Superpowers 看板: could not deliver: {message}"
                        )]),
                    }
                }
                _ => plain(vec![
                    "usage: /superpowers-kanban add <spec> <plan>".to_string(),
                ]),
            }
        }
        _ if argument == "add-prequeued" || argument.starts_with("add-prequeued ") => {
            let title = argument
                .strip_prefix("add-prequeued")
                .map(str::trim)
                .unwrap_or("");
            if title.is_empty() {
                plain(vec!["usage: /superpowers-kanban add-prequeued <title>".to_string()])
            } else {
                match query_plugin(workdir, "enqueue_prequeued", json!({ "title": title })) {
                    Ok(value) => {
                        let id = value.get("id").and_then(Value::as_str).unwrap_or("card");
                        plain(vec![format!("Superpowers 看板: delivered {id} to inbox")])
                    }
                    Err(QueryFailure::PluginMissing) => unavailable(),
                    Err(QueryFailure::Other(message)) => {
                        plain(vec![format!("Superpowers 看板: could not deliver: {message}")])
                    }
                }
            }
        }
        _ if argument == "promote" || argument.starts_with("promote ") => {
            let mut parts = argument.split_whitespace();
            let _verb = parts.next();
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(id), Some(spec), Some(plan), title) => {
                    let mut params = json!({ "id": id, "spec_path": spec, "plan_path": plan });
                    if let Some(title) = title {
                        params["title"] = json!(title);
                    }
                    match query_plugin(workdir, "promote", params) {
                        Ok(value) => {
                            let id = value.get("id").and_then(Value::as_str).unwrap_or(id);
                            plain(vec![format!("Superpowers 看板: delivered promote for {id}")])
                        }
                        Err(QueryFailure::PluginMissing) => unavailable(),
                        Err(QueryFailure::Other(message)) => plain(vec![format!(
                            "Superpowers 看板: could not deliver: {message}"
                        )]),
                    }
                }
                _ => plain(vec![
                    "usage: /superpowers-kanban promote <card-id> <spec> <plan> [<title>]"
                        .to_string(),
                ]),
            }
        }
        _ => plain(vec![
            "usage: /superpowers-kanban [on|off|create|remove|status|run|add <spec> <plan>|add-prequeued <title>|promote <id> <spec> <plan> [<title>]]"
                .to_string(),
        ]),
    }
}

/// 把插件回的卡片数组渲染成若干行。开关状态由插件给，本地不猜。
fn render_board(workdir: &Path, value: &Value) -> Result<Vec<String>, String> {
    let switch =
        query_plugin(workdir, "switch.read", json!({})).map_err(|failure| match failure {
            QueryFailure::PluginMissing => "Superpowers 看板插件未安装。".to_string(),
            QueryFailure::Other(message) => format!("无法读取开关: {message}"),
        })?;
    let on = switch.get("on").and_then(Value::as_bool).unwrap_or(false);
    let source = switch
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let mut lines = vec![format!(
        "Superpowers 看板 {} (source: {source})",
        if on { "on" } else { "off" }
    )];
    if !on {
        lines.push("Superpowers 看板 is disabled. Enable it with /superpowers-kanban on.".into());
        return Ok(lines);
    }
    let Some(cards) = value.get("cards").and_then(Value::as_array) else {
        lines.push("Superpowers 看板 is empty.".into());
        return Ok(lines);
    };
    if cards.is_empty() {
        lines.push("Superpowers 看板 is empty.".into());
        return Ok(lines);
    }
    for card in cards {
        let id = card.get("id").and_then(Value::as_str).unwrap_or("");
        let state = card.get("state").and_then(Value::as_str).unwrap_or("");
        let where_ = card
            .get("workdir")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .or_else(|| card.get("plan_path").and_then(Value::as_str))
            .filter(|path| !path.is_empty())
            .or_else(|| card.get("title").and_then(Value::as_str))
            .unwrap_or("");
        if where_.is_empty() {
            lines.push(format!("{id} {state}"));
        } else {
            lines.push(format!("{id} {state} @ {where_}"));
        }
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};

    /// 起一个假 daemon，按 `respond` 的指示回一个 `PluginResult` 或错误，并记下收到的查询。
    ///
    /// TUI 现在只经 daemon 说话，所以测试必须在 socket 上对测——直接写文件会测不到
    /// 真正的接缝（也更像在测一个已经不存在的世界）。
    fn fake_daemon(
        workdir: &Path,
        respond: fn(&str, &Value) -> Result<Value, String>,
    ) -> Arc<Mutex<Vec<(String, Value)>>> {
        let socket = crate::runtime_socket_for(workdir).unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&log);
        std::thread::spawn(move || {
            for stream in listener.incoming().take(8) {
                let Ok(stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                let command = &request["command"];
                let method = command["method"].as_str().unwrap_or("").to_string();
                let params = command["params"].clone();
                captured
                    .lock()
                    .unwrap()
                    .push((method.clone(), params.clone()));
                let result = match respond(&method, &params) {
                    Ok(value) => json!({ "type": "PluginResult", "value": value }),
                    // The wire code is snake_case (`not_found`), not the Rust
                    // variant name; the wrong spelling fails to parse and the
                    // whole reply would look like a broken daemon.
                    Err(message) => {
                        json!({ "type": "Error", "code": "not_found", "message": message })
                    }
                };
                let reply = json!({
                    "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
                    "request_id": request["request_id"],
                    "result": result,
                });
                let mut stream = stream;
                let _ = stream.write_all(reply.to_string().as_bytes());
                let _ = stream.write_all(b"\n");
            }
        });
        log
    }

    /// 一个装了看板的 daemon：开关开着，队列里有一张 running 卡片。
    fn installed(method: &str, params: &Value) -> Result<Value, String> {
        match method {
            "switch.read" => Ok(json!({ "on": true, "source": "project" })),
            "switch.write" => Ok(json!({ "on": params["on"] })),
            "list" => Ok(json!({ "cards": [
                { "id": "card-1", "state": "running", "plan_path": "a.plan.md" }
            ]})),
            // 插件自己的生命周期动作：宿主只转发，不认识结果形状。
            "lifecycle/create" => Ok(json!({ "project": params["project"], "registered": true })),
            "lifecycle/remove" => Ok(json!({ "project": params["project"], "registered": false })),
            "enqueue" => Ok(json!({ "id": "a-spec-a-plan" })),
            "enqueue_prequeued" => Ok(json!({ "id": "my-idea" })),
            "promote" => Ok(json!({ "id": params["id"] })),
            "import_plugin" => Ok(json!({ "ok": true })),
            other => Err(format!("unknown method: {other}")),
        }
    }

    /// 插件没被守护时 daemon 的回答（措辞与 daemon 侧一致）。
    fn uninstalled(_method: &str, _params: &Value) -> Result<Value, String> {
        Err("plugin superpowers-kanban is not available".to_string())
    }

    /// 开关关着。
    fn disabled_board(method: &str, params: &Value) -> Result<Value, String> {
        match method {
            "switch.read" => Ok(json!({ "on": false, "source": "default" })),
            other => installed(other, params),
        }
    }

    /// daemon 活着、应答正常，但它的插件没起来：这可能是「插件没装」，也可能是
    /// 「本目录没建看板」——回的话一样，只有登记表能分辨。
    fn runtime_up_plugin_down(_method: &str, _params: &Value) -> Result<Value, String> {
        Err("plugin superpowers-kanban is not available".to_string())
    }

    #[test]
    fn a_missing_plugin_still_says_so_even_in_an_unregistered_directory() {
        // 「插件没装」和「本目录没建看板」是两回事：前者要先把插件装上，
        // 所以即便本目录没登记，也不能改口成创建指引（既有测试也钉死这一点）。
        // 只有「连不上 / 插件拒绝」这类读不到，才轮到登记表分辨。
        let dir = tempfile::tempdir().unwrap();
        let resident_dir = dir.path().join("resident");
        let workdir = dir.path().join("proj");
        std::fs::create_dir_all(&workdir).unwrap();
        fake_daemon(&workdir, runtime_up_plugin_down);

        let outcome = handle_kanban_with(&workdir, "", &resident_dir);
        assert!(
            outcome.lines[0].contains("插件未安装"),
            "expected the uninstalled notice, got {:?}",
            outcome.lines
        );
    }

    #[test]
    fn an_unreadable_board_in_an_unregistered_directory_points_at_create() {
        // 本目录没建看板，而且没有 daemon 应答——正是「连不上」的那种读不到。
        // 以前这里甩一句 `daemon is unavailable` 的连接错误，用户没有下一步；
        // 现在给创建指引。故意不起 fake_daemon：连接失败本身就是这个场景。
        let dir = tempfile::tempdir().unwrap();
        let resident_dir = dir.path().join("resident");
        let workdir = dir.path().join("proj");
        std::fs::create_dir_all(&workdir).unwrap();

        let outcome = handle_kanban_with(&workdir, "", &resident_dir);
        assert_eq!(outcome.toggled_to, None);
        assert!(
            outcome.lines[0].contains("未创建"),
            "expected the create-first guidance, got {:?}",
            outcome.lines
        );
        assert!(
            outcome.lines[0].contains("/superpowers-kanban create"),
            "guidance must name the command: {:?}",
            outcome.lines
        );
    }

    #[test]
    fn an_unreadable_board_in_a_registered_directory_still_reports_the_failure() {
        // 反向：本目录**已登记**看板，读不出来就是真故障（daemon 挂了 / 插件
        // 拒绝），必须报错而不是让用户去 create 一个已经存在的看板。
        let dir = tempfile::tempdir().unwrap();
        let resident_dir = dir.path().join("resident");
        let workdir = dir.path().join("proj");
        std::fs::create_dir_all(&workdir).unwrap();
        // 通用的常驻登记：本项目已被某个请求者要求常驻。宿主只问「有没有」，
        // 不认识是哪个插件。
        yi_agent_store::resident::require(&resident_dir, &workdir, "superpowers-kanban").unwrap();

        let outcome = handle_kanban_with(&workdir, "", &resident_dir);
        assert!(
            outcome.lines[0].contains("无法读取看板"),
            "a registered board's read failure must be reported: {:?}",
            outcome.lines
        );
    }

    #[test]
    fn a_corrupt_resident_registry_reads_as_empty_and_points_at_create() {
        // 通用登记目录对坏文件是**宽容读**（回空表），与常驻值守的读法一致：
        // 一份坏文件不该让值守者崩掉，也不该在这里变成一条误导性的错误。
        // 于是「读不到 + 登记为空」就是「本目录还没建」，给创建指引。
        let dir = tempfile::tempdir().unwrap();
        let resident_dir = dir.path().join("resident");
        let workdir = dir.path().join("proj");
        std::fs::create_dir_all(&workdir).unwrap();
        std::fs::create_dir_all(&resident_dir).unwrap();
        std::fs::write(resident_dir.join("resident-daemons.json"), "{ not json").unwrap();

        let outcome = handle_kanban_with(&workdir, "", &resident_dir);
        assert!(
            outcome.lines[0].contains("尚未创建"),
            "a lenient empty read is 'not created yet': {:?}",
            outcome.lines
        );
        assert!(
            !outcome.lines[0].contains("损坏"),
            "the generic registry is read leniently, not reported as corrupt: {:?}",
            outcome.lines
        );
    }

    /// `create`/`remove` 经**通用通道**转给插件：宿主不再自己写登记表。
    #[test]
    fn create_and_remove_are_forwarded_to_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let resident_dir = dir.path().join("resident");

        // 只起一个假 daemon：同一个 workdir 的 socket 只能 bind 一次。
        let log = fake_daemon(dir.path(), installed);
        let outcome = handle_kanban_with(dir.path(), "create", &resident_dir);
        assert_eq!(outcome.toggled_to, None);
        assert!(
            outcome.lines[0].contains("已创建"),
            "create must be acknowledged: {:?}",
            outcome.lines
        );
        let queries = log.lock().unwrap();
        let (method, params) = queries
            .iter()
            .find(|(method, _)| method == "lifecycle/create")
            .unwrap_or_else(|| panic!("create must reach the plugin, saw {queries:?}"));
        assert_eq!(
            params["project"],
            json!(dir.path().to_string_lossy()),
            "the plugin acts on `params.project`"
        );
        drop(queries);

        let outcome = handle_kanban_with(dir.path(), "remove", &resident_dir);
        assert!(
            outcome.lines[0].contains("已移除"),
            "remove must be acknowledged: {:?}",
            outcome.lines
        );
        let queries = log.lock().unwrap();
        assert!(
            queries
                .iter()
                .any(|(method, _)| method == "lifecycle/remove"),
            "remove must reach the plugin, saw {queries:?}"
        );
    }

    #[test]
    fn the_board_renders_cards_the_plugin_returns() {
        let dir = tempfile::tempdir().unwrap();
        fake_daemon(dir.path(), installed);

        let outcome = handle_kanban(dir.path(), "");
        assert_eq!(outcome.toggled_to, None);
        assert!(
            outcome.lines.iter().any(|line| line.contains("card-1")),
            "the live card should be rendered, got {:?}",
            outcome.lines
        );
        assert!(
            outcome.lines.iter().any(|line| line.contains("running")),
            "the card state should be visible, got {:?}",
            outcome.lines
        );
    }

    #[test]
    fn on_writes_the_switch_through_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let log = fake_daemon(dir.path(), installed);

        let outcome = handle_kanban(dir.path(), "on");
        assert_eq!(outcome.toggled_to, Some(true));
        let queries = log.lock().unwrap();
        assert!(
            queries
                .iter()
                .any(|(method, params)| method == "switch.write" && params["on"] == true),
            "the switch write must reach the plugin, saw {queries:?}"
        );
    }

    #[test]
    fn off_writes_the_switch_through_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let log = fake_daemon(dir.path(), installed);

        let outcome = handle_kanban(dir.path(), "off");
        assert_eq!(outcome.toggled_to, Some(false));
        let queries = log.lock().unwrap();
        assert!(
            queries
                .iter()
                .any(|(method, params)| method == "switch.write" && params["on"] == false),
            "saw {queries:?}"
        );
    }

    #[test]
    fn add_delivers_through_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let log = fake_daemon(dir.path(), installed);

        let outcome = handle_kanban(dir.path(), "add a.spec.md a.plan.md");
        assert!(
            outcome.lines[0].contains("delivered"),
            "expected a delivery acknowledgement, got {:?}",
            outcome.lines
        );
        let queries = log.lock().unwrap();
        assert!(
            queries.iter().any(|(method, params)| method == "enqueue"
                && params["spec_path"] == "a.spec.md"
                && params["plan_path"] == "a.plan.md"),
            "saw {queries:?}"
        );
    }

    #[test]
    fn add_prequeued_delivers_through_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let resident_dir = dir.path().join("resident");
        let log = fake_daemon(dir.path(), installed);
        let outcome = handle_kanban_with(dir.path(), "add-prequeued my idea", &resident_dir);
        assert!(outcome.lines[0].contains("my-idea"), "{:?}", outcome.lines);
        let calls = log.lock().unwrap().clone();
        assert!(
            calls
                .iter()
                .any(|(method, params)| method == "enqueue_prequeued"
                    && params["title"] == "my idea"),
            "{calls:?}"
        );
    }

    #[test]
    fn promote_delivers_through_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let resident_dir = dir.path().join("resident");
        let log = fake_daemon(dir.path(), installed);
        let outcome = handle_kanban_with(
            dir.path(),
            "promote idea a.spec.md a.plan.md",
            &resident_dir,
        );
        assert!(outcome.lines[0].contains("idea"), "{:?}", outcome.lines);
        let calls = log.lock().unwrap().clone();
        assert!(
            calls.iter().any(|(method, params)| method == "promote"
                && params["id"] == "idea"
                && params["spec_path"] == "a.spec.md"
                && params["plan_path"] == "a.plan.md"),
            "{calls:?}"
        );
    }

    #[test]
    fn a_board_line_falls_back_to_the_title_when_there_is_no_plan() {
        let dir = tempfile::tempdir().unwrap();
        let resident_dir = dir.path().join("resident");
        fake_daemon(dir.path(), |method, params| match method {
            "switch.read" => Ok(json!({ "on": true, "source": "project" })),
            "list" => Ok(json!({
                "cards": [{
                    "id": "idea", "state": "pre_queued", "title": "一个想法",
                    "plan_path": "", "workdir": ""
                }]
            })),
            other => installed(other, params),
        });
        let outcome = handle_kanban_with(dir.path(), "", &resident_dir);
        assert!(
            outcome
                .lines
                .iter()
                .any(|line| line.contains("idea") && line.contains("一个想法")),
            "{:?}",
            outcome.lines
        );
    }

    #[test]
    fn a_missing_plugin_says_so_instead_of_failing_obscurely() {
        let dir = tempfile::tempdir().unwrap();
        fake_daemon(dir.path(), uninstalled);

        let outcome = handle_kanban(dir.path(), "");
        assert!(
            outcome.lines[0].contains("插件未安装"),
            "expected an uninstalled notice, got {:?}",
            outcome.lines
        );
    }

    #[test]
    fn a_disabled_board_refuses_to_run_and_says_where_the_switch_is() {
        let dir = tempfile::tempdir().unwrap();
        fake_daemon(dir.path(), disabled_board);

        let outcome = handle_kanban(dir.path(), "run");
        assert_eq!(outcome.toggled_to, None);
        assert!(
            outcome.lines[0].contains("disabled"),
            "expected a disabled notice, got {:?}",
            outcome.lines
        );
    }

    #[test]
    fn an_unknown_argument_is_reported_instead_of_silently_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = handle_kanban(dir.path(), "sideways");
        assert_eq!(outcome.toggled_to, None);
        assert!(
            outcome.lines[0].contains("usage"),
            "expected a usage line, got {:?}",
            outcome.lines
        );
    }

    #[test]
    fn add_with_missing_paths_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = handle_kanban(dir.path(), "add only-one-arg");
        assert!(
            outcome.lines[0].contains("usage"),
            "expected a usage line, got {:?}",
            outcome.lines
        );
    }

    #[test]
    fn saving_the_runtime_preference_does_not_drop_the_board_switch() {
        // `runtime_prefs::save` 与看板开关共用 preferences.json：保存前者绝不能
        // 抹掉后者的键（看板开关现在由插件写，但同一个文件仍必须保住这个键）。
        let dir = tempfile::tempdir().unwrap();
        crate::tui::runtime_prefs::save(
            dir.path(),
            crate::tui::runtime_prefs::RuntimePreference::Always,
        )
        .unwrap();
        let path = crate::tui::runtime_prefs::preferences_path(dir.path());
        let mut object = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| value.as_object().cloned())
            .unwrap();
        object.insert("superpowers_kanban".into(), serde_json::Value::Bool(true));
        std::fs::write(&path, serde_json::to_string(&object).unwrap()).unwrap();

        crate::tui::runtime_prefs::save(
            dir.path(),
            crate::tui::runtime_prefs::RuntimePreference::Never,
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["superpowers_kanban"], true,
            "a runtime-pref save must not clobber the board switch in preferences.json"
        );
    }
}
