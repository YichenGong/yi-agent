use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};
use yi_agent_skills::SkillsService;

pub struct SkillTool {
    service: Arc<SkillsService>,
}

#[derive(Debug, Deserialize)]
struct SkillArgs {
    path: String,
}

impl SkillTool {
    pub fn new(service: Arc<SkillsService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "Skill"
    }

    fn description(&self) -> &str {
        "Load full instructions for a skill. Call this when you need detailed guidance from a skill listed in the available-skills section of the system prompt. Pass the exact path shown in the catalog."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the skill's SKILL.md file, as shown in the available-skills section."
                }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let parsed: SkillArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
        };
        match self.service.load_skill_body(&parsed.path) {
            Ok(body) => ToolResult::text(format!(
                "<skill path=\"{}\">\n{}\n</skill>",
                parsed.path, body
            )),
            Err(e) => ToolResult::error(format!("skill load failed: {e}")),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Plugin {
                name: "skills".to_string(),
            },
            requires_confirmation: false,
            read_only: true,
            version: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_service_with_file(content: &str) -> (tempfile::TempDir, Arc<SkillsService>) {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("SKILL.md");
        std::fs::write(&path, content).unwrap();
        let svc = Arc::new(SkillsService::new(vec![]));
        (tmp, svc)
    }

    #[tokio::test]
    async fn call_loads_skill_body() {
        let (tmp, svc) = make_service_with_file("---\nname: foo\ndescription: x\n---\nbody text");
        let path = tmp.path().join("SKILL.md");
        let tool = SkillTool::new(svc);
        let args = serde_json::json!({ "path": path.to_str().unwrap() });
        let result = tool.call(args).await;
        assert!(!result.is_error);
        let text = match &result.content[0] {
            yi_agent_core::ContentBlock::Text(s) => s,
            _ => panic!("expected text content"),
        };
        assert!(text.contains("body text"));
        assert!(text.contains("<skill path="));
    }

    #[tokio::test]
    async fn call_nonexistent_path_errors() {
        let svc = Arc::new(SkillsService::new(vec![]));
        let tool = SkillTool::new(svc);
        let args = serde_json::json!({ "path": "/nonexistent/SKILL.md" });
        let result = tool.call(args).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn call_missing_path_arg_errors() {
        let svc = Arc::new(SkillsService::new(vec![]));
        let tool = SkillTool::new(svc);
        let args = serde_json::json!({});
        let result = tool.call(args).await;
        assert!(result.is_error);
    }
}
