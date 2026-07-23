use std::path::PathBuf;
use yi_agent_core::ToolResult;

/// All errors produced by builtin tools.
/// Converted to `ToolResult::error(...)` at the boundary so the agent loop
/// can feed them back to the LLM.
#[derive(Debug, thiserror::Error)]
pub enum ToolsError {
    #[error("path escapes root: {0}")]
    PathEscapesRoot(PathBuf),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("file not found: {0}")]
    NotFound(PathBuf),

    #[error("edit failed: {reason}")]
    EditFailed { reason: String },

    #[error("command blocked by safety filter: {0}")]
    CommandBlocked(String),

    #[error("command timeout after {0}s")]
    Timeout(u64),

    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),

    #[error("glob pattern error: {0}")]
    Glob(#[from] glob::PatternError),

    #[error("args parse error: {0}")]
    ArgsParse(#[from] serde_json::Error),

    #[error("http error: {0}")]
    Http(String),

    #[error("unsupported content type: {0}")]
    UnsupportedContentType(String),

    #[error("response too large: {0} bytes")]
    ResponseTooLarge(usize),

    #[error("search engine error: {0}")]
    SearchEngine(String),

    #[error("BOCHA_API_KEY not set")]
    MissingApiKey,
}

impl From<ToolsError> for ToolResult {
    fn from(e: ToolsError) -> Self {
        ToolResult::error(e.to_string())
    }
}
