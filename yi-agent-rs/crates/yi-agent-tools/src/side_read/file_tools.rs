//! Read-only file tools for a side conversation.
//!
//! These are the side counterparts of `read` / `read_document` / `view_image` /
//! `glob` / `grep`. They differ from the ordinary tools in one decisive way:
//! every path is opened through a [`SideReadAccess`] handle rather than by
//! string, so a revoked root, a swapped symlink or a path race cannot be
//! exploited. The bytes are then read from the **already-opened** handle, which
//! is what makes "reads the file that was authorised" true rather than hoped for.
//!
//! The tool metadata is explicit: `Builtin` source and `read_only`, so the side
//! whitelist check (`Builtin && read_only`) can accept them and nothing else.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use yi_agent_core::{
    ContentBlock, ImageDetail, ImageSource, Tool, ToolMetadata, ToolRegistry, ToolResult,
    ToolSource,
};

use crate::error::ToolsError;
use crate::image_prep::{self, PreparedImage};

use super::SideReadAccess;
use super::blocking::SideBlockingPool;

/// Default number of lines returned by the side `read` tool.
const DEFAULT_READ_LIMIT: usize = 2000;
/// Never return more than this many bytes from one side read.
const MAX_READ_BYTES: usize = 2 * 1024 * 1024;
/// Ceiling for the `limit` argument, so a caller cannot ask for everything.
const MAX_READ_LINES: usize = 20_000;
/// Files larger than this are refused before any parse work starts.
const MAX_DOCUMENT_BYTES: usize = 50 * 1024 * 1024;
/// Images are capped by `image_prep`, but refuse absurd sizes early.
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Bound on candidates any recursive side scan will consider.
const MAX_SCAN_CANDIDATES: usize = 20_000;
/// Bound on recursion depth for side scans.
const MAX_SCAN_DEPTH: usize = 32;
/// Wall-clock budget for one side scan.
const SCAN_DEADLINE: Duration = Duration::from_secs(10);
/// Bound on a single grep hit's rendered line.
const MAX_GREP_LINE_BYTES: usize = 4096;
/// Bound on the number of grep hits returned.
const MAX_GREP_HITS: usize = 1000;

/// Everything a side file tool needs: authorisation, bounded blocking work, and
/// the lifetime it belongs to.
#[derive(Clone)]
pub struct SideFileToolContext {
    pub access: Arc<SideReadAccess>,
    pub pool: Arc<SideBlockingPool>,
    pub side_id: String,
    pub cancel: CancellationToken,
    pub owner: Arc<dyn Send + Sync>,
}

impl SideFileToolContext {
    /// Run a blocking job under this side's admission and cancellation.
    async fn run_blocking<T, F>(&self, job: F) -> Result<T, ToolsError>
    where
        T: Send + 'static,
        F: FnOnce() -> std::io::Result<T> + Send + 'static,
    {
        self.pool
            .run(
                &self.side_id,
                self.cancel.clone(),
                Arc::clone(&self.owner),
                job,
            )
            .await
            .map_err(ToolsError::Io)
    }
}

/// Read a file from an already-opened handle, bounded by `limit` bytes.
///
/// `take(limit + 1)` detects the real overflow instead of trusting a metadata
/// length that may have changed since it was read.
fn read_bounded_handle(mut file: std::fs::File, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    file.seek(SeekFrom::Start(0))?;
    file.take(limit as u64 + 1).read_to_end(&mut buffer)?;
    if buffer.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file is larger than the {limit}-byte side read limit"),
        ));
    }
    Ok(buffer)
}

// ---------------------------------------------------------------------------
// read
// ---------------------------------------------------------------------------

pub struct SideReadTool {
    ctx: SideFileToolContext,
}

impl SideReadTool {
    pub fn new(ctx: SideFileToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for SideReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file from the side conversation's authorised roots. Returns lines \
         with line numbers; use `offset`/`limit` to page through a large file."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Relative or absolute path inside the authorised roots" },
                "offset": { "type": "integer", "description": "1-based first line to return" },
                "limit": { "type": "integer", "description": "Maximum number of lines to return" }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: ReadArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(error) => return ToolsError::ArgsParse(error).into(),
        };
        let access = Arc::clone(&self.ctx.access);
        let path = PathBuf::from(&args.path);
        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args
            .limit
            .unwrap_or(DEFAULT_READ_LIMIT)
            .clamp(1, MAX_READ_LINES);

        let result = self
            .ctx
            .run_blocking(move || {
                // The handle is opened under authorisation and the bytes come
                // from it, so replacing the path afterwards cannot redirect the
                // read.
                let file = access.open_regular(&path)?;
                read_bounded_handle(file, MAX_READ_BYTES)
            })
            .await;
        match result {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let lines: Vec<&str> = text.lines().collect();
                let total = lines.len();
                if total == 0 {
                    return ToolResult::text(format!("read {} (empty file)", args.path));
                }
                if offset > total {
                    return ToolResult::error(format!(
                        "offset {offset} is past the end of {} ({total} lines)",
                        args.path
                    ));
                }
                let start = offset - 1;
                let end = start.saturating_add(limit).min(total);
                let mut body = String::new();
                for (index, line) in lines[start..end].iter().enumerate() {
                    let number = start + index + 1;
                    body.push_str(&format!("{number:>6}\t{line}\n"));
                }
                if end < total {
                    body.push_str(&format!(
                        "\n[showed lines {}-{} of {total}; continue with offset={}]",
                        start + 1,
                        end,
                        end + 1
                    ));
                }
                ToolResult::text(body)
            }
            Err(error) => error.into(),
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

// ---------------------------------------------------------------------------
// read_document
// ---------------------------------------------------------------------------

pub struct SideReadDocumentTool {
    ctx: SideFileToolContext,
}

impl SideReadDocumentTool {
    pub fn new(ctx: SideFileToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ReadDocumentArgs {
    path: String,
    #[serde(default)]
    pages: Option<String>,
}

#[async_trait]
impl Tool for SideReadDocumentTool {
    fn name(&self) -> &str {
        "read_document"
    }

    fn description(&self) -> &str {
        "Read a document from the side conversation's authorised roots: PDF (text \
         layer only), DOCX, HTML, or plain text. `pages` selects PDF pages."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Relative or absolute path inside the authorised roots" },
                "pages": { "type": "string", "description": "PDF pages, e.g. \"3\" or \"1-5\"" }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: ReadDocumentArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(error) => return ToolsError::ArgsParse(error).into(),
        };
        let access = Arc::clone(&self.ctx.access);
        let path = PathBuf::from(&args.path);
        let pages = args.pages.clone();
        let display = args.path.clone();

        let result = self
            .ctx
            .run_blocking(move || {
                let file = access.open_regular(&path)?;
                let bytes = read_bounded_handle(file, MAX_DOCUMENT_BYTES)?;
                let extension = Path::new(&display)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_string();
                crate::fs::read_document::read_document_bytes(
                    &bytes,
                    &display,
                    &extension,
                    pages.as_deref(),
                    100_000,
                )
                .map_err(|error| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
                })
            })
            .await;
        match result {
            Ok(text) => ToolResult::text(text),
            Err(error) => error.into(),
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

// ---------------------------------------------------------------------------
// view_image
// ---------------------------------------------------------------------------

pub struct SideViewImageTool {
    ctx: SideFileToolContext,
}

impl SideViewImageTool {
    pub fn new(ctx: SideFileToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ViewImageArgs {
    path: String,
    #[serde(default)]
    detail: Option<String>,
}

#[async_trait]
impl Tool for SideViewImageTool {
    fn name(&self) -> &str {
        "view_image"
    }

    fn description(&self) -> &str {
        "View an image from the side conversation's authorised roots."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Relative or absolute path inside the authorised roots" },
                "detail": { "type": "string", "enum": ["high", "original"], "description": "Image detail level" }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: ViewImageArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(error) => return ToolsError::ArgsParse(error).into(),
        };
        let access = Arc::clone(&self.ctx.access);
        let path = PathBuf::from(&args.path);
        let detail = match args.detail.as_deref() {
            Some("original") => ImageDetail::Original,
            _ => ImageDetail::High,
        };
        let budget = image_prep::resolve_budget();

        let result = self
            .ctx
            .run_blocking(move || {
                let file = access.open_regular(&path)?;
                let bytes = read_bounded_handle(file, MAX_IMAGE_BYTES)?;
                image_prep::prepare_image_bytes(&bytes, detail, budget)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            })
            .await;
        match result {
            Ok(PreparedImage::Ready {
                media_type,
                data,
                width,
                height,
                note,
            }) => {
                let label = match note {
                    Some(note) => format!(
                        "viewed {} ({width}x{height}, {media_type}, {note})",
                        args.path
                    ),
                    None => format!("viewed {} ({width}x{height}, {media_type})", args.path),
                };
                ToolResult::with_content(vec![
                    ContentBlock::Text(label),
                    ContentBlock::Image {
                        source: ImageSource::Base64 { media_type, data },
                        detail,
                        path: Some(args.path.clone()),
                    },
                ])
            }
            Ok(PreparedImage::Omitted { message }) => {
                ToolResult::text(format!("viewed {} ({message})", args.path))
            }
            Err(error) => error.into(),
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

// ---------------------------------------------------------------------------
// glob
// ---------------------------------------------------------------------------

pub struct SideGlobTool {
    ctx: SideFileToolContext,
}

impl SideGlobTool {
    pub fn new(ctx: SideFileToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for SideGlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files by glob pattern inside the side conversation's authorised roots."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern, e.g. \"**/*.rs\"" },
                "path": { "type": "string", "description": "Directory to search in (defaults to the side's root)" }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: GlobArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(error) => return ToolsError::ArgsParse(error).into(),
        };
        let access = Arc::clone(&self.ctx.access);
        let base = args.path.clone().unwrap_or_else(|| ".".to_string());
        let pattern = args.pattern.clone();

        let result = self
            .ctx
            .run_blocking(move || scan(access.as_ref(), Path::new(&base), &pattern))
            .await;
        match result {
            Ok(matches) => {
                if matches.is_empty() {
                    return ToolResult::text(format!("no files matched {:?}", args.pattern));
                }
                let mut body = matches.join("\n");
                body.push_str(&format!("\n\n{} file(s)", matches.len()));
                ToolResult::text(body)
            }
            Err(error) => error.into(),
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

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

pub struct SideGrepTool {
    ctx: SideFileToolContext,
}

impl SideGrepTool {
    pub fn new(ctx: SideFileToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for SideGrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents by regular expression inside the side conversation's \
         authorised roots."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression" },
                "path": { "type": "string", "description": "Directory to search in (defaults to the side's root)" }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: GrepArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(error) => return ToolsError::ArgsParse(error).into(),
        };
        if let Err(error) = regex::Regex::new(&args.pattern) {
            return ToolsError::Regex(error).into();
        }
        let access = Arc::clone(&self.ctx.access);
        let base = args.path.clone().unwrap_or_else(|| ".".to_string());
        let pattern = args.pattern.clone();

        let result = self
            .ctx
            .run_blocking(move || grep(access.as_ref(), Path::new(&base), &pattern))
            .await;
        match result {
            Ok(hits) => {
                if hits.is_empty() {
                    return ToolResult::text(format!("no matches for {:?}", args.pattern));
                }
                let mut body = hits.join("\n");
                body.push_str(&format!("\n\n{} match(es)", hits.len()));
                ToolResult::text(body)
            }
            Err(error) => error.into(),
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

/// Register the five read-only side file tools.
pub fn register_side_file_tools(
    registry: &mut ToolRegistry,
    access: Arc<SideReadAccess>,
    pool: Arc<SideBlockingPool>,
    side_id: String,
    cancel: CancellationToken,
    owner: Arc<dyn Send + Sync>,
) {
    let ctx = SideFileToolContext {
        access,
        pool,
        side_id,
        cancel,
        owner,
    };
    registry.register(Arc::new(SideReadTool::new(ctx.clone())));
    registry.register(Arc::new(SideReadDocumentTool::new(ctx.clone())));
    registry.register(Arc::new(SideViewImageTool::new(ctx.clone())));
    registry.register(Arc::new(SideGlobTool::new(ctx.clone())));
    registry.register(Arc::new(SideGrepTool::new(ctx)));
}

// ---------------------------------------------------------------------------
// safe recursive scan
// ---------------------------------------------------------------------------

/// Walk from `base` using directory **handles**, re-authorising every entry.
///
/// `walkdir` plus a later `File::open(path)` would reopen an unchecked path, so
/// the traversal itself goes through `SideReadAccess::open_directory`. Every
/// candidate is bounded by count, depth and deadline, and re-checked before use.
fn walk_candidates(
    access: &SideReadAccess,
    base: &Path,
    mut visit: impl FnMut(&Path, std::fs::File) -> bool,
) -> std::io::Result<()> {
    let deadline = Instant::now() + SCAN_DEADLINE;
    let mut candidates = 0usize;
    let mut pending: Vec<(PathBuf, usize)> = vec![(base.to_path_buf(), 0)];

    while let Some((dir, depth)) = pending.pop() {
        if depth > MAX_SCAN_DEPTH || Instant::now() > deadline {
            break;
        }
        let entries = read_dir_entries(access, &dir)?;
        for (name, is_dir) in entries {
            candidates += 1;
            if candidates > MAX_SCAN_CANDIDATES || Instant::now() > deadline {
                return Ok(());
            }
            let child = dir.join(&name);
            if is_dir {
                // Re-authorised when it is actually walked.
                pending.push((child, depth + 1));
                continue;
            }
            // Re-authorise and open the exact candidate.
            let Ok(file) = access.open_regular(&child) else {
                continue;
            };
            if !visit(&child, file) {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// List one directory through its authorised handle.
///
/// The listing comes from the handle that was opened under authorisation, never
/// from a re-resolved path (see [`unix::read_dir_handle`]).
fn read_dir_entries(
    access: &SideReadAccess,
    dir: &Path,
) -> std::io::Result<Vec<(std::ffi::OsString, bool)>> {
    let handle = access.open_directory(dir)?;
    let entries = super::unix::read_dir_handle(&handle)?;
    Ok(entries
        .into_iter()
        .map(|entry| (entry.name, entry.is_dir))
        .collect())
}

/// Glob within the authorised roots.
fn scan(access: &SideReadAccess, base: &Path, pattern: &str) -> std::io::Result<Vec<String>> {
    let matcher = glob::Pattern::new(pattern).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
    })?;
    let mut matches = Vec::new();
    walk_candidates(access, base, |path, _file| {
        let relative = path
            .strip_prefix(access.primary())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        if matcher.matches(&relative) || matcher.matches(&path.to_string_lossy()) {
            matches.push(relative);
        }
        matches.len() < MAX_GREP_HITS
    })?;
    matches.sort();
    Ok(matches)
}

/// Regex search within the authorised roots.
fn grep(access: &SideReadAccess, base: &Path, pattern: &str) -> std::io::Result<Vec<String>> {
    let regex = regex::Regex::new(pattern).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
    })?;
    let mut hits = Vec::new();
    walk_candidates(access, base, |path, file| {
        let Ok(bytes) = read_bounded_handle(file, MAX_READ_BYTES) else {
            return true;
        };
        let text = String::from_utf8_lossy(&bytes);
        let relative = path
            .strip_prefix(access.primary())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        for (index, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                let mut rendered = line.trim_end().to_string();
                if rendered.len() > MAX_GREP_LINE_BYTES {
                    let mut cut = MAX_GREP_LINE_BYTES;
                    while cut > 0 && !rendered.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    rendered.truncate(cut);
                    rendered.push('…');
                }
                hits.push(format!("{relative}:{}\t{rendered}", index + 1));
                if hits.len() >= MAX_GREP_HITS {
                    return false;
                }
            }
        }
        true
    })?;
    Ok(hits)
}
