# yi-agent-tools Implementation Design

**Date:** 2026-07-19
**Status:** Approved (brainstormed 2026-07-19)
**Scope:** `yi-agent-rs/crates/yi-agent-tools`

## Goal

在 `yi-agent-tools` 中实现 6 个内置工具,覆盖 coding agent 的 FS + Shell 核心能力:

| 工具 | 作用 |
|---|---|
| `Read` | 读取文件(支持 offset/limit 行范围) |
| `Write` | 创建或整体写入文件 |
| `Edit` | old_string → new_string 局部替换 |
| `Glob` | glob pattern 匹配文件路径 |
| `Grep` | 正则搜索文件内容,多种输出模式 |
| `Bash` | sh -c 执行命令,黑名单 + timeout |

Web 工具(搜索/抓取)不在本轮范围。

## Decisions (brainstorming 结论)

| # | 决策点 | 选择 | 理由 |
|---|---|---|---|
| 1 | 范围 | FS 5 + Shell(共 6 个),Web 延后 | 核心骨架能力;Web 决策点独立 |
| 2 | 路径安全 | 单一 root 限制,canonicalize + starts_with | 防 `../` 逃逸,与 Claude Code `CLAUDE_PROJECT_DIR` 一致 |
| 3 | 状态组织 | 统一 `ToolsContext { root, cwd: Mutex<PathBuf> }`,工具存 `Arc<ToolsContext>` | root 只存一份;扩展权限模型只改一处 |
| 4 | 注册 API | 顶层函数 `register_builtin_tools(&mut ToolRegistry, PathBuf)` | 调用方一行注册,最小样板 |
| 5 | Shell 模型 | `sh -c` + 黑名单 + timeout 120s + 输出截断 100KB | 不过度限制;拦高危模式;防 hang |
| 6 | Edit 模型 | old_string / new_string 唯一匹配替换 | 与 Claude Code 一致;防 Write 整体重写漂移 |
| 7 | Grep 语义 | 正则 + 多模式输出(content/files_with_matches/count)+ glob 过滤 | 与 Claude Code Grep 对齐 |
| 8 | Glob 语法 | glob crate(支持 `**` 递归) | 与 Unix 约定/LLM 训练数据一致 |
| 9 | 测试 | tempfile + tokio::test | 纯本地,CI 可跑 |
| 10 | 元数据 | 区分 read_only + requires_confirmation | Write/Edit/Bash 设 `requires_confirmation=true`;Read/Glob/Grep 设 `read_only=true` |
| 11 | 错误处理 | thiserror enum + 转 `ToolResult::error(human_readable)` | LLM 可理解错误原因自我修复 |
| 12 | Shell 输出 | stdout/stderr 各截断到 100KB,保留尾部,标记截断 | 防塞爆 LLM 上下文 |
| 13 | Shell cwd | 持久 cwd(`Mutex<PathBuf>`),解析 `cd` 命令同步状态 | 跨调用保持工作目录,体验更好 |
| 14 | Sandbox | 延后单独一轮设计 | 跨平台方案复杂,与 tools 实现解耦 |

## Architecture

### 模块结构

```
yi-agent-rs/crates/yi-agent-tools/
├── Cargo.toml
└── src/
    ├── lib.rs                          # pub use + register_builtin_tools()
    ├── context.rs                      # ToolsContext { root, cwd: Mutex<PathBuf> }
    ├── error.rs                        # ToolsError (thiserror) + From<ToolsError> for ToolResult
    ├── fs/
    │   ├── mod.rs                      # pub mod read/write/edit/glob/grep;
    │   ├── read.rs                     # ReadTool + impl Tool
    │   ├── write.rs                    # WriteTool + impl Tool
    │   ├── edit.rs                     # EditTool + impl Tool
    │   ├── glob.rs                     # GlobTool + impl Tool
    │   ├── grep.rs                     # GrepTool + impl Tool
    │   └── path_util.rs                # resolve_and_check(root, path) -> Result<PathBuf>
    ├── shell/
    │   ├── mod.rs                      # pub mod bash; pub use bash::BashTool;
    │   ├── bash.rs                     # BashTool + impl Tool + execute()
    │   └── blocklist.rs                # is_blocked(cmd: &str) -> Option<&'static str>
    └── tests/
        ├── fs_tests.rs                 # tempfile 集成测试
        └── shell_tests.rs              # tempfile + tokio::test
```

### 数据流

**FS 工具通用流程:**

```
LLM 调 tool(args: Value)
  → 解析 args 为强类型 Args struct(serde_json::from_value)
  → path_util::resolve_and_check(ctx.root, &args.path)
      → root.join(&path)
      → canonicalize
      → 校验 starts_with(root.canonicalize())
      → 失败返回 ToolsError::PathEscapesRoot → ToolResult::error(...)
  → 执行工具逻辑
  → 成功返回 ToolResult::text(...) 或 ToolResult::with_content(...)
  → IO 错误转 ToolsError::Io → ToolResult::error(...)
```

**Shell 工具流程:**

```
LLM 调 BashTool(args: Value)
  → 解析 args { command: String, timeout?: u64 }
  → blocklist::is_blocked(&cmd)?
      → 命中黑名单 → ToolResult::error("blocked: ...")
  → cwd = ctx.cwd.lock().clone()  // 读取当前 cwd
  → tokio::process::Command::new("sh")
      .arg("-c").arg(&cmd)
      .current_dir(&cwd)
      .stdin(Null)  // 不接受输入,防 hang
      .stdout(Piped).stderr(Piped)
      .spawn()
  → tokio::time::timeout(Duration::from_secs(timeout), child.wait_with_output())
      → 超时 → kill child → ToolResult::error("timeout after {N}s")
  → 截断 stdout/stderr 各到 100KB(保留尾部)
  → 更新 ctx.cwd = new_cwd(解析 `cd <dir>` 命令的最后目标)
  → 返回 ToolResult::text(format!("exit: {}\nstdout:\n{}\nstderr:\n{}"))
```

### cwd 保持逻辑

ShellTool 解析命令里的 `cd <dir>`(可能多次),用最后一次的 dir 更新 `ctx.cwd`。不执行 `cd` 本身(sh -c 会处理),但工具层"观察"cd 意图并同步状态。

- `cd foo && make` → cwd 更新为 `root/foo`(相对路径解析为 cwd.join)
- `cd /abs/path` → cwd 更新为 `/abs/path`(但若不在 root 内,下次执行会失败,因为 `current_dir` 不校验)
- 无 cd → cwd 保持不变

## Types & Error Handling

### ToolsContext

```rust
pub struct ToolsContext {
    root: PathBuf,
    cwd: Mutex<PathBuf>,
}

impl ToolsContext {
    pub fn new(root: PathBuf) -> Self {
        let cwd = root.clone();
        Self { root, cwd: Mutex::new(cwd) }
    }
    pub fn root(&self) -> &Path { &self.root }
    pub fn cwd(&self) -> PathBuf { self.cwd.lock().unwrap().clone() }
    pub fn set_cwd(&self, path: PathBuf) { *self.cwd.lock().unwrap() = path; }
}
```

### 各工具 Args(serde Deserialize)

```rust
// fs/read.rs
struct ReadArgs { path: String, offset: Option<usize>, limit: Option<usize> }
// fs/write.rs
struct WriteArgs { path: String, content: String }
// fs/edit.rs
struct EditArgs { path: String, old_string: String, new_string: String }
// fs/glob.rs
struct GlobArgs { pattern: String, path: Option<String> }  // path 默认 root
// fs/grep.rs
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    output_mode: Option<OutputMode>,  // content/files_with_matches/count,默认 content
    context: Option<usize>,          // 匹配行前后各 N 行
}
// shell/bash.rs
struct BashArgs { command: String, timeout: Option<u64> }  // 默认 120s
```

### ToolsError(thiserror)

```rust
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
}

impl From<ToolsError> for ToolResult {
    fn from(e: ToolsError) -> Self { ToolResult::error(e.to_string()) }
}
```

### 错误返回策略

所有工具内部用 `Result<T, ToolsError>`,调用末尾 `.map_err(ToolResult::from)` 或 `?` 后转。LLM 拿到的是人类可读错误字符串,能理解原因自我修复。

## FS Tools

### ReadTool

**schema:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Relative or absolute path within root" },
    "offset": { "type": "integer", "description": "Line number to start from (1-based), default 1" },
    "limit": { "type": "integer", "description": "Max lines to read, default 2000" }
  },
  "required": ["path"]
}
```

**行为:**
- `offset` 1-based,默认 1
- `limit` 默认 2000 行(对齐 Claude Code),超出截断并标记 `[truncated: showed {N} of {M} lines]`
- 输出加行号前缀(`cat -n` 格式),对齐 Claude Code 风格
- 文件不存在 → `ToolsError::NotFound`
- 是目录 → `ToolsError::Io("is a directory")`
- `metadata`: `read_only = true`

**输出示例:**
```
     1→fn main() {
     2→    println!("hello");
     3→}
```

### WriteTool

**schema:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string" },
    "content": { "type": "string" }
  },
  "required": ["path", "content"]
}
```

**行为:**
- 创建或覆盖文件(父目录不存在则创建,`create_dir_all`)
- 不修改同名已存在文件除非显式调用(永远覆盖)
- 成功返回 `ToolResult::text("wrote {N} bytes to {path}")`
- `metadata`: `requires_confirmation = true`

### EditTool

**schema:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string" },
    "old_string": { "type": "string", "description": "Unique text to find" },
    "new_string": { "type": "string", "description": "Text to replace with" }
  },
  "required": ["path", "old_string", "new_string"]
}
```

**行为:**
- 读全文,做字符串替换
- **匹配 0 次** → `ToolsError::EditFailed { reason: "old_string not found" }`
- **匹配 ≥2 次** → `ToolsError::EditFailed { reason: "old_string matched {N} times, must be unique" }`
- **匹配恰好 1 次** → 替换,写回文件
- `old_string == new_string` → `ToolsError::EditFailed { reason: "old_string equals new_string" }`(no-op 防御)
- `old_string` 为空 → `ToolsError::EditFailed { reason: "old_string is empty" }`
- 成功返回 `ToolResult::text("edited {path}: replaced 1 occurrence")`
- `metadata`: `requires_confirmation = true`

### 关键设计点

- Read 的行号格式与 Claude Code 一致(`cat -n` 风格),LLM 训练数据熟悉
- Write 的"永远覆盖"语义明确,避免歧义;LLM 想局部改用 Edit
- Edit 的唯一匹配约束 + 多种错误情况都在错误信息里说明,LLM 能据此调整 old_string 重试

## Glob/Grep/Bash

### GlobTool

**schema:**
```json
{
  "type": "object",
  "properties": {
    "pattern": { "type": "string", "description": "Glob pattern, supports ** for recursive" },
    "path": { "type": "string", "description": "Base directory, default root" }
  },
  "required": ["pattern"]
}
```

**行为:**
- 用 `glob` crate,base = `path` 或 root
- pattern 相对 base 解析:`**/*.rs` 匹配所有 .rs 文件
- 返回匹配的路径列表(相对 root 的路径,每行一个)
- 路径数量无上限(LLM 自己消化)
- 0 匹配 → 返回空文本 `ToolResult::text("no matches")`(不是错误)
- `metadata`: `read_only = true`

**输出示例:**
```
src/main.rs
src/lib.rs
src/tools/read.rs
```

### GrepTool

**schema:**
```json
{
  "type": "object",
  "properties": {
    "pattern": { "type": "string", "description": "Regex pattern" },
    "path": { "type": "string", "description": "File or directory, default root" },
    "glob": { "type": "string", "description": "Filter files by glob, e.g. *.rs" },
    "output_mode": {
      "type": "string",
      "enum": ["content", "files_with_matches", "count"],
      "default": "content"
    },
    "context": { "type": "integer", "description": "Lines of context around matches, default 0" }
  },
  "required": ["pattern"]
}
```

**行为:**
- 正则用 `regex` crate
- 遍历用 `walkdir` crate(递归遍历目录)
- `glob` 过滤:用 `glob::Pattern::matches_path` 对每个文件名过滤
- `output_mode`:
  - `content` → 每个匹配 `<file>:<line>:<content>`,带行号;`context > 0` 时显示前后各 N 行
  - `files_with_matches` → 只列文件名,每行一个
  - `count` → 每个文件 `<file>:<count>`
- 二进制文件跳过(读前 1KB 检测 NUL 字节)
- 0 匹配 → `ToolResult::text("no matches")`(不是错误)
- `metadata`: `read_only = true`

### BashTool

**schema:**
```json
{
  "type": "object",
  "properties": {
    "command": { "type": "string", "description": "Shell command to execute" },
    "timeout": { "type": "integer", "description": "Timeout in seconds, default 120" }
  },
  "required": ["command"]
}
```

**行为:**
1. 解析 `cd <dir>` 命令,记录最后一次 cd 目标(正则 `cd\s+(\S+)`)
2. `blocklist::is_blocked(&command)` → 命中返回 `ToolResult::error("blocked: {reason}")`
3. `cwd = ctx.cwd()` 读取当前 cwd
4. `Command::new("sh").arg("-c").arg(&cmd).current_dir(&cwd).stdin(Stdio::null()).stdout(Piped).stderr(Piped).spawn()`
5. `tokio::time::timeout(Duration::from_secs(timeout), child.wait_with_output())`
   - 超时 → `child.kill()`,返回 `ToolResult::error("command timeout after {N}s")`
6. 截断 stdout/stderr:> 100KB 保留尾部,前缀加 `[truncated: showed last 100KB of {M}B]`
7. 更新 `ctx.set_cwd(new_cwd)`(若有 cd 命令)
8. 输出格式:
   ```
   exit: 0
   stdout:
   <stdout>
   stderr:
   <stderr>
   ```
- `metadata`: `requires_confirmation = true`

### Blocklist(shell/blocklist.rs)

高危命令黑名单(正则匹配):

| 模式 | 理由 |
|---|---|
| `rm\s+-rf?\s+/(--)?\s*$` | 删根目录 |
| `rm\s+-rf?\s+~/` | 删 home 目录 |
| `rm\s+-rf?\s+\$HOME` | 删 home(环境变量形式) |
| `:\(\)\{\s*:\|\:&\s*\};:` | fork bomb |
| `mkfs(\.\w+)?\s+/dev/` | 格式化磁盘 |
| `dd\s+.*of=/dev/[a-z]` | 写块设备 |
| `>\s*/dev/sd[a-z]` | 直接写块设备 |
| `>\s*/dev/nvme` | 直接写 NVMe 设备 |
| `git\s+push\s+(-f|--force)\s+.*\b(main|master)\b` | 强推主分支 |
| `git\s+push\s+(-f|--force)\s+origin\s+(main|master)` | 强推主分支 |
| `curl\s+.*\|\s*(sh|bash|zsh)` | 管道执行远程脚本 |
| `wget\s+.*\|\s*(sh|bash|zsh)` | 同上 |
| `chmod\s+-R\s+0+` | 递归清空权限 |
| `chown\s+-R\s+.*:.*\s+/` | 递归改根目录属主 |
| `shutdown\s+` | 关机 |
| `reboot\s+` | 重启 |
| `halt\s+` | 停机 |
| `poweroff\s+` | 关机 |
| `init\s+0` | 关机 |
| `kill\s+-9\s+-1` | kill 所有进程 |
| `killall\s+-9` | kill 所有同名进程 |
| `pkill\s+-9` | 同上 |
| `iptables\s+-F` | 清空防火墙规则 |
| `ufw\s+disable` | 关防火墙 |
| `systemctl\s+(stop|disable)\s+` | 停/禁服务 |
| `launchctl\s+(unload|stop)\s+` | macOS 停服务 |
| `defaults\s+delete\s+` | macOS 删除系统偏好设置 |
| `npm\s+publish` | 发布 npm 包(误发布会污染注册表) |
| `cargo\s+publish` | 发布 crates.io 包(同上) |
| `docker\s+rm\s+-f\s+` | 强删容器 |
| `docker\s+rmi\s+-f\s+` | 强删镜像 |
| `truncate\s+-s\s+0\s+/dev/sd` | 清零块设备 |

```rust
pub fn is_blocked(cmd: &str) -> Option<&'static str> {
    static PATTERNS: &[(Regex, &str)] = &[
        // (regex, reason) pairs
        // 第一个参数编译好的正则,第二个是 reason 字符串
    ];
    for (re, reason) in PATTERNS {
        if re.is_match(cmd) { return Some(reason); }
    }
    None
}
```

## Dependencies

### `yi-agent-tools/Cargo.toml`

```toml
[package]
name = "yi-agent-tools"
description = "Built-in tools: filesystem, shell, web"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[dependencies]
yi-agent-core = { workspace = true }

# 异步
async-trait = "0.1"
tokio = { version = "1", features = ["process", "time", "fs", "io-util"] }
futures = "0.3"  # Tool trait 依赖

# 序列化
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# 错误
thiserror = "2"

# FS 工具
glob = "0.3"
walkdir = "2"
regex = "1"

[dev-dependencies]
tempfile = "3"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

### 设计要点

- `tokio` 只拉需要的 features:`process`(Shell)、`time`(timeout)、`fs`(async FS)、`io-util`
- `[dev-dependencies]` 里 tokio 加 `macros` + `rt-multi-thread` 供测试用
- `futures` 是 Tool trait 依赖(core 已拉,但显式声明避免隐式依赖)
- `regex` 和 `glob` 版本选当前稳定版
- 不引入 `tracing` — YAGNI

## Registration API

```rust
// lib.rs
mod context;
mod error;
mod fs;
mod shell;

pub use context::ToolsContext;
pub use error::ToolsError;
pub use fs::{ReadTool, WriteTool, EditTool, GlobTool, GrepTool};
pub use shell::BashTool;

/// 注册全部内置工具到 registry。
/// root: 工具操作的工作目录根,FS 工具只能在此目录内操作,
///       Shell 工具以此为初始 cwd(但不限制 sh -c 的操作范围)。
pub fn register_builtin_tools(registry: &mut ToolRegistry, root: PathBuf) {
    let ctx = Arc::new(ToolsContext::new(root));
    registry.register(Arc::new(ReadTool::new(ctx.clone())));
    registry.register(Arc::new(WriteTool::new(ctx.clone())));
    registry.register(Arc::new(EditTool::new(ctx.clone())));
    registry.register(Arc::new(GlobTool::new(ctx.clone())));
    registry.register(Arc::new(GrepTool::new(ctx.clone())));
    registry.register(Arc::new(BashTool::new(ctx)));
}

impl ReadTool {
    pub fn new(ctx: Arc<ToolsContext>) -> Self { Self { ctx } }
}
// ... 其他工具同构
```

## Testing

用 `tempfile::TempDir` + `tokio::test`。

### FS 工具测试覆盖

- `read_file_basic` — 读小文件,验证行号格式
- `read_file_offset_limit` — offset/limit 参数
- `read_file_truncated` — 超过 2000 行的截断标记
- `read_not_found` → `ToolsError::NotFound`
- `read_directory` → 错误
- `path_escape_attempt` — `../` 逃逸被拦截
- `write_new_file` — 创建新文件
- `write_create_parent_dirs` — 父目录自动创建
- `write_overwrite` — 覆盖已有文件
- `edit_unique_match` — 唯一匹配替换
- `edit_multiple_matches` → 错误
- `edit_no_match` → 错误
- `edit_empty_old_string` → 错误
- `edit_same_strings` → 错误
- `glob_recursive` — `**/*.rs` 递归匹配
- `glob_no_matches` — 空结果不是错误
- `grep_content_mode` — content 输出带行号
- `grep_files_mode` — 只列文件名
- `grep_count_mode` — 每文件计数
- `grep_glob_filter` — `*.rs` 过滤
- `grep_binary_skip` — 二进制文件跳过
- `grep_context` — context 行数

### Shell 工具测试覆盖

- `bash_basic_echo` — echo 输出
- `bash_exit_code` — 非零退出码正确返回
- `bash_stderr` — stderr 捕获
- `bash_cwd_persist` — `cd foo` 后下次调用 cwd 已更新
- `bash_timeout` — sleep 超过 timeout 被 kill
- `bash_blocklist_rm_rf` — `rm -rf /` 被拦
- `bash_blocklist_fork_bomb` — fork bomb 被拦
- `bash_blocklist_force_push_main` — 强推 main 被拦
- `bash_output_truncated` — 大输出截断到 100KB
- `bash_stdin_null` — stdin 不接受输入

## Implementation Order

(供 writing-plans 参考,非本设计文档约束)

1. `context.rs` — ToolsContext(无依赖,先做)
2. `error.rs` — ToolsError + From<ToolsError> for ToolResult
3. `fs/path_util.rs` — resolve_and_check
4. `fs/read.rs` — ReadTool(最简单,验证 Tool trait 流程)
5. `fs/write.rs` — WriteTool
6. `fs/edit.rs` — EditTool
7. `fs/glob.rs` — GlobTool
8. `fs/grep.rs` — GrepTool(依赖 walkdir + regex)
9. `shell/blocklist.rs` — 黑名单
10. `shell/bash.rs` — BashTool(依赖 blocklist + tokio::process)
11. `lib.rs` — register_builtin_tools
12. tests — tempfile 集成测试

## Out of Scope (YAGNI)

- **Sandbox**(跨平台进程隔离)— 单独一轮设计
- **Web 工具**(搜索/抓取)— 单独一轮
- **Shell 不受 root 约束**:当前 root 只保护 FS 工具,Shell 可操作系统任意路径。依赖 `requires_confirmation=true` + 用户确认 + 未来 sandbox
- **白名单**:不做命令白名单
- **环境变量注入**:Shell 继承父进程 env,不提供 env 配置
- **stdin 输入**:永远 Stdio::null()
- **持久化会话 cwd**:cwd 只在 ToolsContext 内存中,agent 重启后重置为 root
- **ripgrep 子进程**:用 walkdir + regex 自实现,不起 rg 子进程
- **glob 大规模优化**:不限制返回路径数量

## 安全注意事项

**当前设计不提供系统级隔离。** Shell 工具执行 `sh -c` 可访问系统任意路径(root 限制只对 FS 工具生效)。安全性依赖:
1. `requires_confirmation=true` 元数据(CLI 端必须读此标记弹确认)
2. 黑名单拦截已知高危模式
3. timeout 120s 防死循环
4. 用户监督

真正的系统级隔离由后续 sandbox 设计提供。
