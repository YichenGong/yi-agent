use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

use crate::process::manager::{OnExitPolicy, ProcessManager, ProcessSelector, ProcessStartOptions};

const DEFAULT_READ_MAX_BYTES: usize = 64 * 1024;

pub struct ProcessStartTool {
    manager: Arc<ProcessManager>,
}

impl ProcessStartTool {
    pub fn new(manager: Arc<ProcessManager>) -> Self {
        Self { manager }
    }
}

#[derive(Debug, Deserialize)]
struct ProcessStartArgs {
    command: String,
    name: Option<String>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    on_exit: OnExitPolicy,
    ready_pattern: Option<String>,
    ready_timeout_sec: Option<u64>,
}

#[async_trait]
impl Tool for ProcessStartTool {
    fn name(&self) -> &str {
        "process_start"
    }

    fn description(&self) -> &str {
        "Start a managed background process and optionally wait for readiness output."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to run as a managed background process."
                },
                "name": {
                    "type": "string",
                    "description": "Optional unique human-friendly process name."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional working directory, absolute or relative to the workspace root."
                },
                "env": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Optional environment variables to set for the process."
                },
                "on_exit": {
                    "type": "string",
                    "enum": ["kill", "keep"],
                    "default": "kill",
                    "description": "Shutdown policy for this process when the agent exits."
                },
                "ready_pattern": {
                    "type": "string",
                    "description": "Optional stdout/stderr substring that marks the process ready."
                },
                "ready_timeout_sec": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Seconds to wait for ready_pattern before returning with a warning."
                }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: ProcessStartArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(err) => return ToolResult::error(format!("invalid arguments: {err}")),
        };

        let opts = ProcessStartOptions {
            command: args.command,
            name: args.name,
            cwd: args.cwd,
            env: args.env,
            on_exit: args.on_exit,
            ready_pattern: args.ready_pattern,
            ready_timeout_sec: args.ready_timeout_sec,
        };

        match self.manager.start(opts).await {
            Ok(result) => json_result(&result),
            Err(err) => ToolResult::error(err),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Builtin,
            requires_confirmation: true,
            read_only: false,
            version: None,
        }
    }
}

pub struct ProcessListTool {
    manager: Arc<ProcessManager>,
}

impl ProcessListTool {
    pub fn new(manager: Arc<ProcessManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for ProcessListTool {
    fn name(&self) -> &str {
        "process_list"
    }

    fn description(&self) -> &str {
        "List managed background processes."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn call(&self, _args: Value) -> ToolResult {
        json_result(&self.manager.list())
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Builtin,
            requires_confirmation: false,
            read_only: true,
            version: None,
        }
    }
}

pub struct ProcessReadTool {
    manager: Arc<ProcessManager>,
}

impl ProcessReadTool {
    pub fn new(manager: Arc<ProcessManager>) -> Self {
        Self { manager }
    }
}

#[derive(Debug, Deserialize)]
struct ProcessReadArgs {
    process_id: Option<String>,
    name: Option<String>,
    cursor: Option<u64>,
    max_bytes: Option<usize>,
}

#[async_trait]
impl Tool for ProcessReadTool {
    fn name(&self) -> &str {
        "process_read"
    }

    fn description(&self) -> &str {
        "Read buffered stdout and stderr from a managed background process."
    }

    fn schema(&self) -> Value {
        process_selector_schema(serde_json::json!({
            "cursor": {
                "type": "integer",
                "minimum": 0,
                "description": "Optional output cursor returned by a previous process_read call."
            },
            "max_bytes": {
                "type": "integer",
                "minimum": 1,
                "default": DEFAULT_READ_MAX_BYTES,
                "description": "Maximum bytes to return from each stream. Defaults to 64 KiB."
            }
        }))
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: ProcessReadArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(err) => return ToolResult::error(format!("invalid arguments: {err}")),
        };
        let selector = match selector_from_args(args.process_id, args.name) {
            Ok(selector) => selector,
            Err(err) => return ToolResult::error(err),
        };
        let max_bytes = args.max_bytes.unwrap_or(DEFAULT_READ_MAX_BYTES);
        if max_bytes == 0 {
            return ToolResult::error("max_bytes must be greater than 0");
        }

        match self.manager.read(selector, args.cursor, max_bytes).await {
            Ok(result) => json_result(&result),
            Err(err) => ToolResult::error(err),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Builtin,
            requires_confirmation: false,
            read_only: true,
            version: None,
        }
    }
}

pub struct ProcessKillTool {
    manager: Arc<ProcessManager>,
}

impl ProcessKillTool {
    pub fn new(manager: Arc<ProcessManager>) -> Self {
        Self { manager }
    }
}

#[derive(Debug, Deserialize)]
struct ProcessKillArgs {
    process_id: Option<String>,
    name: Option<String>,
}

#[async_trait]
impl Tool for ProcessKillTool {
    fn name(&self) -> &str {
        "process_kill"
    }

    fn description(&self) -> &str {
        "Kill a managed background process by process_id or name."
    }

    fn schema(&self) -> Value {
        process_selector_schema(serde_json::json!({}))
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: ProcessKillArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(err) => return ToolResult::error(format!("invalid arguments: {err}")),
        };
        let selector = match selector_from_args(args.process_id, args.name) {
            Ok(selector) => selector,
            Err(err) => return ToolResult::error(err),
        };

        match self.manager.kill(selector).await {
            Ok(()) => ToolResult::text("killed"),
            Err(err) => ToolResult::error(err),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Builtin,
            requires_confirmation: true,
            read_only: false,
            version: None,
        }
    }
}

fn selector_from_args(
    process_id: Option<String>,
    name: Option<String>,
) -> Result<ProcessSelector, String> {
    match (process_id, name) {
        (Some(process_id), None) => Ok(ProcessSelector::Id(process_id)),
        (None, Some(name)) => Ok(ProcessSelector::Name(name)),
        (None, None) => Err("exactly one of process_id or name is required".to_string()),
        (Some(_), Some(_)) => Err("provide exactly one of process_id or name".to_string()),
    }
}

fn process_selector_schema(extra_properties: Value) -> Value {
    let mut properties = serde_json::json!({
        "process_id": {
            "type": "string",
            "description": "Managed process id returned by process_start or process_list."
        },
        "name": {
            "type": "string",
            "description": "Unique managed process name."
        }
    });
    if let (Some(base), Some(extra)) = (properties.as_object_mut(), extra_properties.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }

    serde_json::json!({
        "type": "object",
        "properties": properties
    })
}

fn json_result<T: serde::Serialize>(value: &T) -> ToolResult {
    match serde_json::to_string_pretty(value) {
        Ok(json) => ToolResult::text(json),
        Err(err) => ToolResult::error(format!("failed to serialize result: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::Value;
    use tempfile::TempDir;
    use yi_agent_core::{ContentBlock, Tool};

    use crate::process::manager::ProcessManager;

    use super::{ProcessKillTool, ProcessListTool, ProcessReadTool, ProcessStartTool};

    fn text(result: &yi_agent_core::ToolResult) -> &str {
        match &result.content[0] {
            ContentBlock::Text(text) => text,
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn process_tools_start_list_read_kill() {
        let temp = TempDir::new().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        let start = ProcessStartTool::new(manager.clone());
        let list = ProcessListTool::new(manager.clone());
        let read = ProcessReadTool::new(manager.clone());
        let kill = ProcessKillTool::new(manager);

        let started = start
            .call(serde_json::json!({
                "command": "printf ready; sleep 2",
                "name": "dev-server",
                "ready_pattern": "ready",
                "ready_timeout_sec": 2
            }))
            .await;
        assert!(!started.is_error, "{}", text(&started));
        let start_json: Value = serde_json::from_str(text(&started)).unwrap();
        let process_id = start_json["process_id"].as_str().unwrap().to_string();

        let listed = list.call(serde_json::json!({})).await;
        assert!(!listed.is_error, "{}", text(&listed));
        assert!(text(&listed).contains("dev-server"));

        let read_result = read.call(serde_json::json!({ "name": "dev-server" })).await;
        assert!(!read_result.is_error, "{}", text(&read_result));
        assert!(text(&read_result).contains("ready"));

        let killed = kill
            .call(serde_json::json!({ "process_id": process_id }))
            .await;
        assert!(!killed.is_error, "{}", text(&killed));
        assert_eq!(text(&killed), "killed");
    }

    #[tokio::test]
    async fn process_read_rejects_zero_max_bytes() {
        let temp = TempDir::new().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        let read = ProcessReadTool::new(manager);

        let result = read
            .call(serde_json::json!({
                "process_id": "proc_1",
                "max_bytes": 0
            }))
            .await;

        assert!(result.is_error);
        assert!(text(&result).contains("max_bytes must be greater than 0"));
    }

    #[test]
    fn process_selector_schemas_are_openai_compatible() {
        let temp = TempDir::new().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(ProcessReadTool::new(manager.clone())),
            Box::new(ProcessKillTool::new(manager)),
        ];

        for tool in tools {
            let schema = tool.schema();
            assert_eq!(schema["type"], "object", "{}", tool.name());
            for keyword in ["oneOf", "anyOf", "allOf", "enum", "const", "not"] {
                assert!(
                    schema.get(keyword).is_none(),
                    "{} contains {keyword}",
                    tool.name()
                );
            }
        }
    }

    #[test]
    fn process_tool_metadata_matches_permissions() {
        let temp = TempDir::new().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());

        let start = ProcessStartTool::new(manager.clone()).metadata();
        assert!(start.requires_confirmation);
        assert!(!start.read_only);

        let list = ProcessListTool::new(manager.clone()).metadata();
        assert!(!list.requires_confirmation);
        assert!(list.read_only);

        let read = ProcessReadTool::new(manager.clone()).metadata();
        assert!(!read.requires_confirmation);
        assert!(read.read_only);

        let kill = ProcessKillTool::new(Arc::clone(&manager)).metadata();
        assert!(kill.requires_confirmation);
        assert!(!kill.read_only);
    }
}
