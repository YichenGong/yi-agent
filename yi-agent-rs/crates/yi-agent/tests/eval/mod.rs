//! Golden-set eval runner：评「结果」与「轨迹」。
//!
//! 相对于 `e2e_complex.rs`（只断言结果文件），这里额外断言**过程**：
//! 该调的工具调了没有、不该调的多调了没有、步数与 token 有没有失控。
//! 这才能抓到「工具选错但碰巧把文件写出来」这类失败。
//!
//! 轨迹信息全部来自 `yi-agent run --json` 的 JSONL 事件流，无需改 core：
//! `AgentEvent::ToolCall { id, name, input }` / `ToolResult` / `ToolExit` /
//! `Usage { model, usage: TokenUsage { input_tokens, .. } }`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 一条 eval case 的完整定义，对应一个 TOML 文件。
#[derive(Debug, serde::Deserialize)]
pub struct Case {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub fixture: Fixture,
    pub expect: Expect,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct Fixture {
    /// 相对路径 → 文件内容。runner 写到临时目录后再跑 agent。
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct Expect {
    pub result: ResultExpect,
    pub trajectory: TrajectoryExpect,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct ResultExpect {
    /// 相对路径 → 期望内容的正则。文件必须存在且匹配。
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct TrajectoryExpect {
    /// 必须调用过的工具名。
    ///
    /// 只在「该工具是完成任务**唯一可接受的手段**」时才用它。想要表达
    /// 「文件必须被改过」请用结果断言（`expect.result.files`）+ `must_call_any`：
    /// 实测 agent 覆写同一文件时 write 与 edit 都会用，而两者都能达成目标，
    /// 写成 `must_call = ["write"]` 会把正确实现判成失败（case 008 即如此）。
    #[serde(default)]
    pub must_call: Vec<String>,
    /// 必须**真的改动过文件**（write / edit，或 bash 里带文件重定向的写）。
    ///
    /// 为什么不用 `must_call_any = ["write","edit"]`：实测三轮全量里有 3 条
    /// case（005/006/007）被这么判成失败，而 agent 实际是用
    /// `cat > src/a.rs <<'EOF' ... EOF`、`printf 'new\n' > README.md`
    /// 把文件改对的——结果断言全部通过。在这个 agent 里 bash 重定向是一等
    /// 的写文件手段，把工具名当契约等于把正确实现判成失败。
    ///
    /// 它比只看结果强的地方：能抓到「一步工具都没调却声称改好了」，
    /// 也能抓到「只读不写但结果恰好已符合」（那种情况结果断言会过，
    /// 但轨迹说明模型没在干活）。
    #[serde(default)]
    pub must_modify_files: bool,
    /// 绝不允许调用的工具名。
    #[serde(default)]
    pub must_not_call: Vec<String>,
    /// 工具调用总数上限。
    pub max_tool_calls: Option<usize>,
    /// 累计 input token 上限。
    pub max_input_tokens: Option<u64>,
    /// 不允许出现在任何 bash 命令里的正则（对命令原文匹配）。
    ///
    /// 为什么需要它：`must_not_call = ["bash"]` 只能表达「不许开 shell」，
    /// 但本模型读文件时习惯用 bash（`cat`/`ls`/`grep`），把 bash 一律禁掉
    /// 得到的是假红，而不是安全问题。真正该禁的是**具体危险命令**
    /// （`rm -rf`、`sudo rm` 之类），所以按命令内容匹配。
    #[serde(default)]
    pub forbidden_bash_patterns: Vec<String>,
}

// ---------------------------------------------------------------------------
// 轨迹
// ---------------------------------------------------------------------------

/// 从 JSONL 事件流里抽出评测需要的轨迹信息。
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct Trace {
    /// 按发生顺序的工具名。
    pub tool_calls: Vec<String>,
    /// 累计 input token（各次 `Usage` 事件求和）。
    pub input_tokens: u64,
    /// 是否正常结束（`Done` 且 reason 为 `EndTurn`）。
    pub ended_normally: bool,
    /// 每次 bash 调用的 `command` 原文（按发生顺序）。
    pub bash_commands: Vec<String>,
}

impl Trace {
    /// 这条轨迹是否真的改动过文件。
    ///
    /// 判定口径：调了 `write` / `edit`，或者 bash 命令里有**指向真实文件**的
    /// 重定向。排除 `/dev/null`、`/dev/stderr|stdout|tty` 和 `2>&1` 这类
    /// 纯粹用来吞输出/合并流的写法——它们不改任何文件。
    pub fn modified_files(&self) -> bool {
        if self
            .tool_calls
            .iter()
            .any(|t| matches!(t.as_str(), "write" | "edit"))
        {
            return true;
        }
        self.bash_commands
            .iter()
            .any(|c| bash_writes_file(c) || bash_edits_in_place(c))
    }
}

/// bash 命令是否用**原地编辑**改了文件（`sed -i`、`perl -pi`、`awk -i inplace`）。
///
/// 为什么要单独认这一条：实测 case 005 里 agent 用
/// `sed -i '' 's/calc/compute/g' src/main.rs`、`perl -pi -e ...` 完成了重命名，
/// 结果断言全过，但既没有 write/edit 调用、也没有 `>` 重定向，被
/// `must_modify_files` 判成「没改文件」。这是假红。
///
/// 只认「确实会落盘」的写法：`sed -i` 必须有 -i；`perl -pi` 必须有 -i。
/// 不带 -i 的 `sed`/`perl` 只输出到 stdout，不算改文件。
pub fn bash_edits_in_place(cmd: &str) -> bool {
    let has = |needle: &str| cmd.contains(needle);
    // sed -i / sed -i'' / sed -i.bak / sed --in-place
    let sed_inplace = has("sed ") && (has(" -i") || has("--in-place"));
    // perl -pi / perl -i -pe / perl -i.bak -pe
    let perl_inplace = has("perl ") && (has(" -pi") || has(" -i ") || has(" -i.") || has(" -i'"));
    let awk_inplace = has("awk ") && has("inplace");
    sed_inplace || perl_inplace || awk_inplace
}

/// bash 命令里是否存在「写到真实文件」的重定向。
///
/// 只看 `>` / `>>` 后面的目标：`/dev/null` 与 `/dev/std*` 是丢弃输出，
/// `&1` `&2` 是 fd 复制，都不改文件。目标必须以 `/` 或可见字符开头且不是
/// 这些特例。
pub fn bash_writes_file(cmd: &str) -> bool {
    let bytes = cmd.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'>' {
            let mut j = i + 1;
            // 跳过 `>>` 与可能存在的空白
            if j < bytes.len() && bytes[j] == b'>' {
                j += 1;
            }
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            // fd 复制：`>&2` / `>&1`
            if j < bytes.len() && bytes[j] == b'&' {
                i = j + 1;
                continue;
            }
            let rest = &cmd[j..];
            let target: String = rest
                .chars()
                .take_while(|c| !c.is_whitespace() && !matches!(c, ';' | '|' | '&' | ')'))
                .collect();
            let t = target.trim_matches(|c| c == '\'' || c == '"');
            if !t.is_empty() && !is_discard_target(t) {
                return true;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    false
}

fn is_discard_target(t: &str) -> bool {
    matches!(
        t,
        "/dev/null" | "/dev/stdout" | "/dev/stderr" | "/dev/tty" | "-"
    ) || t.starts_with("/dev/null")
        || t.starts_with("/dev/fd/")
}

/// 从已解析的事件里提取轨迹。
///
/// 事件有两种编码：字符串变体（`"Done"`）和单键对象（`{"ToolCall": {...}}`）。
/// 这里两种都处理，与 `common::event_variant` 的判定口径一致。
pub fn extract_trace(events: &[serde_json::Value]) -> Trace {
    let mut trace = Trace::default();
    for ev in events {
        let Some(obj) = ev.as_object() else {
            if ev.as_str() == Some("Done") {
                trace.ended_normally = true;
            }
            continue;
        };
        if let Some(tc) = obj.get("ToolCall")
            && let Some(name) = tc.get("name").and_then(|v| v.as_str())
        {
            trace.tool_calls.push(name.to_string());
            if name == "bash"
                && let Some(cmd) = tc
                    .get("input")
                    .and_then(|i| i.get("command"))
                    .and_then(|c| c.as_str())
            {
                trace.bash_commands.push(cmd.to_string());
            }
        }
        if let Some(usage) = obj.get("Usage").and_then(|u| u.get("usage"))
            && let Some(n) = usage.get("input_tokens").and_then(|v| v.as_u64())
        {
            trace.input_tokens += n;
        }
        if let Some(done) = obj.get("Done")
            && done.get("reason").and_then(|v| v.as_str()) == Some("EndTurn")
        {
            trace.ended_normally = true;
        }
    }
    trace
}

/// 按 `Expect` 校验轨迹，返回所有违反项（空 = 通过）。
pub fn check_trajectory(expect: &TrajectoryExpect, trace: &Trace) -> Vec<String> {
    let mut failures = Vec::new();
    if !trace.ended_normally {
        failures.push("未正常结束（缺 Done/EndTurn）".to_string());
    }
    for required in &expect.must_call {
        if !trace.tool_calls.iter().any(|t| t == required) {
            failures.push(format!(
                "must_call 未满足：没有调用 {required}（实际：{:?}）",
                trace.tool_calls
            ));
        }
    }
    if expect.must_modify_files && !trace.modified_files() {
        failures.push(format!(
            "must_modify_files 未满足：没有任何工具改动过文件（实际调用：{:?}）",
            trace.tool_calls
        ));
    }
    for forbidden in &expect.must_not_call {
        if trace.tool_calls.iter().any(|t| t == forbidden) {
            failures.push(format!(
                "must_not_call 被违反：调用了 {forbidden}（实际：{:?}）",
                trace.tool_calls
            ));
        }
    }
    if let Some(max) = expect.max_tool_calls
        && trace.tool_calls.len() > max
    {
        failures.push(format!(
            "工具调用数 {} 超过上限 {max}（实际：{:?}）",
            trace.tool_calls.len(),
            trace.tool_calls
        ));
    }
    for pattern in &expect.forbidden_bash_patterns {
        let re = match regex::Regex::new(pattern) {
            Ok(re) => re,
            Err(e) => {
                failures.push(format!(
                    "forbidden_bash_patterns 里的正则非法 {pattern}：{e}"
                ));
                continue;
            }
        };
        if let Some(cmd) = trace.bash_commands.iter().find(|c| re.is_match(c)) {
            failures.push(format!("bash 命令命中禁止模式 /{pattern}/：{cmd}"));
        }
    }
    if let Some(max) = expect.max_input_tokens
        && trace.input_tokens > max
    {
        failures.push(format!("input token {} 超过上限 {max}", trace.input_tokens));
    }
    failures
}

/// 加载 `dir` 下全部 `*.toml`，按文件名排序。
pub fn load_cases(dir: &Path) -> Vec<(PathBuf, Case)> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("读 cases 目录 {} 失败：{e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("toml"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let raw = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("读 {} 失败：{e}", p.display()));
            let case: Case =
                toml::from_str(&raw).unwrap_or_else(|e| panic!("解析 {} 失败：{e}", p.display()));
            (p, case)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 执行
// ---------------------------------------------------------------------------

/// 在临时目录里铺好 fixture，跑 agent，返回 Trace 与 stdout。
pub fn run_case(bin: &Path, case: &Case) -> Result<(Trace, String), String> {
    // 先清空上一轮的轨迹：超时/启动失败时 `run_case` 直接返回 `Err`，
    // 若不清空，调用方会打印出上一条 case 的轨迹，把它当成本条实测值去校准预算。
    // 实测踩过：case 007 超时，打印的 tokens/calls 与 006 完全相同。
    clear_trace();
    let tmp = tempfile::TempDir::new().map_err(|e| e.to_string())?;
    for (rel, content) in &case.fixture.files {
        let path = tmp.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, content).map_err(|e| e.to_string())?;
    }

    let mut cmd = std::process::Command::new(bin);
    cmd.arg("--workdir")
        .arg(tmp.path())
        .arg("run")
        .arg("--json")
        .arg(&case.prompt);
    // 超时重试：实测上游 API 会偶发长时间不返回（同一 case 上轮 11s、下轮 >300s，
    // 进程采样显示阻塞在 socket poll 上等响应，不是死锁）。这类抖动是评测环境
    // 的噪声，直接判 case 失败会把「API 慢」记成「agent 做错了」。
    // 重试 1 次仍然超时才记为失败（重试也失败说明确实不是瞬时抖动）。
    let timeout = std::time::Duration::from_secs(300);
    let out = match crate::common::run_command_with_timeout(&mut cmd, timeout) {
        Ok(out) => out,
        Err(first) => {
            eprintln!("  RETRY {} 首次超时/失败：{first}", case.id);
            crate::common::run_command_with_timeout(&mut cmd, timeout)
                .map_err(|second| format!("{first}\n（重试后仍然失败）{second}"))?
        }
    };

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let events = crate::common::parse_events(&stdout);
    let trace = extract_trace(&events);
    store_trace(&trace);

    let mut failures = check_trajectory(&case.expect.trajectory, &trace);
    for (rel, pattern) in &case.expect.result.files {
        let path = tmp.path().join(rel);
        match std::fs::read_to_string(&path) {
            Ok(actual) => {
                let re =
                    regex::Regex::new(pattern).map_err(|e| format!("{rel} 的正则非法：{e}"))?;
                if !re.is_match(&actual) {
                    failures.push(format!("{rel} 内容不匹配 /{pattern}/，实际：{actual}"));
                }
            }
            Err(e) => failures.push(format!("{rel} 读取失败：{e}")),
        }
    }
    if !failures.is_empty() {
        return Err(format!(
            "case {} 失败：\n  {}",
            case.id,
            failures.join("\n  ")
        ));
    }
    Ok((trace, stdout))
}

// ---------------------------------------------------------------------------
// 上一轮的轨迹（失败时也要能拿到，用于校准预算）
// ---------------------------------------------------------------------------

use std::sync::Mutex;

static LAST_TRACE: Mutex<Option<Trace>> = Mutex::new(None);

/// 最近一次 `run_case` 提取到的轨迹。
///
/// `run_case` 在断言失败时返回 `Err`，但校准 `max_input_tokens` / `max_tool_calls`
/// 恰恰要看失败那次的实测值，所以轨迹单独存一份。每个测试二进制是独立进程、
/// eval 串行跑 case，故用进程内静态量足够。
pub fn last_trace() -> Option<Trace> {
    LAST_TRACE.lock().ok().and_then(|g| g.clone())
}

fn clear_trace() {
    if let Ok(mut slot) = LAST_TRACE.lock() {
        *slot = None;
    }
}

fn store_trace(trace: &Trace) {
    if let Ok(mut slot) = LAST_TRACE.lock() {
        *slot = Some(trace.clone());
    }
}
