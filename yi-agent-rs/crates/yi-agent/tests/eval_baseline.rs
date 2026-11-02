//! Golden-set eval 的驱动入口。
//!
//! 全部 `#[ignore]`：真钱（真实 API 调用）+ 分钟级耗时，不进 `just ci`。
//! 跑法：`cargo test -p yi-agent --test eval_baseline -- --ignored --nocapture`
//! 或 `just eval`。

mod common;
mod eval;

use std::path::PathBuf;

fn cases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/eval/cases")
}

#[test]
#[ignore]
fn golden_set_eval() {
    if !common::skip_if_no_key() {
        return;
    }
    let filter = std::env::var("EVAL_FILTER").unwrap_or_default();
    let cases = eval::load_cases(&cases_dir());
    assert!(!cases.is_empty(), "cases 目录里没有 case");

    let bin = common::yi_agent_bin();
    let mut failures = Vec::new();
    let mut ran = 0usize;

    for (path, case) in &cases {
        if !filter.is_empty() && !case.id.contains(&filter) {
            continue;
        }
        ran += 1;
        let started = std::time::Instant::now();
        let (verdict, detail, trace) = match eval::run_case(&bin, case) {
            Ok((trace, _)) => ("PASS", String::new(), Some(trace)),
            Err(msg) => ("FAIL", msg, eval::last_trace()),
        };
        // 无论成败都打印实测轨迹：max_input_tokens / max_tool_calls 必须来自实测
        // （见 Spec C4），没有这行就只能靠猜，门禁要么永不触发要么频繁假失败。
        // 机器可读，便于把一轮输出直接转成 case 的预算。
        if let Some(t) = &trace {
            println!(
                "TRACE {} {} tools={:?} tokens={} secs={}",
                verdict,
                case.id,
                t.tool_calls,
                t.input_tokens,
                started.elapsed().as_secs()
            );
            // bash 命令要打出来，否则「用了 bash 但没调 edit」这类轨迹失败只能靠猜：
            // 是 agent 用 `sed -i` 绕过工具（真缺陷），还是它把 `cat` 当 `read`
            // 用（提示词问题）。这两种处置完全不同，不能混为一谈。
            if !t.bash_commands.is_empty() {
                println!("  BASH {:?}", t.bash_commands);
            }
        } else {
            println!(
                "TRACE {} {} tools=<none> secs={}",
                verdict,
                case.id,
                started.elapsed().as_secs()
            );
        }
        if verdict == "FAIL" {
            println!("FAIL {}  (from {})", case.id, path.display());
            failures.push(detail);
        }
    }

    println!("\n跑了 {ran} 条 case，失败 {} 条", failures.len());
    assert!(failures.is_empty(), "失败详情：\n{}", failures.join("\n\n"));
}

/// runner 自身逻辑的单测，不需要 API key（默认就跑）。
#[test]
fn extract_trace_reads_tool_calls_usage_and_done() {
    let jsonl = concat!(
        r#"{"ToolCall":{"id":"1","name":"read","input":{}}}"#,
        "\n",
        r#"{"ToolCall":{"id":"2","name":"edit","input":{}}}"#,
        "\n",
        r#"{"Usage":{"model":"m","usage":{"input_tokens":120,"output_tokens":9}}}"#,
        "\n",
        r#"{"Usage":{"model":"m","usage":{"input_tokens":80,"output_tokens":3}}}"#,
        "\n",
        r#"{"Done":{"reason":"EndTurn"}}"#,
        "\n",
    );
    let events = common::parse_events(jsonl);
    let trace = eval::extract_trace(&events);
    assert_eq!(trace.tool_calls, vec!["read", "edit"]);
    assert_eq!(trace.input_tokens, 200);
    assert!(trace.ended_normally);
}

#[test]
fn check_trajectory_reports_every_violation() {
    use eval::{TrajectoryExpect, check_trajectory};
    let trace = eval::Trace {
        tool_calls: vec!["read".into(), "bash".into()],
        input_tokens: 500,
        ended_normally: false,
        bash_commands: vec!["rm -rf /tmp/xyz".into()],
    };
    let expect = TrajectoryExpect {
        must_call: vec!["edit".into()],
        must_modify_files: true,
        must_not_call: vec!["bash".into()],
        max_tool_calls: Some(1),
        max_input_tokens: Some(100),
        forbidden_bash_patterns: vec![r"rm\s+-rf".into()],
    };
    let failures = check_trajectory(&expect, &trace);
    // 未正常结束 / must_call 缺 edit / must_modify_files 未满足 /
    // 误调 bash / 调用数超限 / bash 命中禁止模式 / token 超限
    assert_eq!(failures.len(), 7, "实际：{failures:#?}");
}

#[test]
fn extract_trace_keeps_bash_commands_in_order() {
    let jsonl = concat!(
        r#"{"ToolCall":{"id":"1","name":"bash","input":{"command":"ls -la"}}}"#,
        "\n",
        r#"{"ToolCall":{"id":"2","name":"read","input":{"path":"a.txt"}}}"#,
        "\n",
        r#"{"ToolCall":{"id":"3","name":"bash","input":{"command":"cat a.txt"}}}"#,
        "\n",
        r#"{"Done":{"reason":"EndTurn"}}"#,
        "\n",
    );
    let events = common::parse_events(jsonl);
    let trace = eval::extract_trace(&events);
    assert_eq!(trace.tool_calls, vec!["bash", "read", "bash"]);
    assert_eq!(trace.bash_commands, vec!["ls -la", "cat a.txt"]);
    assert!(trace.ended_normally);
}

#[test]
fn forbidden_bash_pattern_only_matches_dangerous_commands() {
    use eval::{TrajectoryExpect, check_trajectory};
    let expect = TrajectoryExpect {
        forbidden_bash_patterns: vec![r"rm\s+-r".into()],
        ..Default::default()
    };
    // 读操作不该被安全断言误伤
    let benign = eval::Trace {
        tool_calls: vec!["bash".into()],
        bash_commands: vec!["cat keep.txt".into()],
        ended_normally: true,
        input_tokens: 10,
    };
    assert!(check_trajectory(&expect, &benign).is_empty());

    let dangerous = eval::Trace {
        tool_calls: vec!["bash".into()],
        bash_commands: vec!["sudo rm -rf /tmp/x".into()],
        ended_normally: true,
        input_tokens: 10,
    };
    let failures = check_trajectory(&expect, &dangerous);
    assert_eq!(failures.len(), 1, "实际：{failures:#?}");
    assert!(failures[0].contains("命中禁止模式"), "实际：{failures:#?}");
}

/// `must_modify_files` + `bash_writes_file` 的语义。
///
/// 这两个存在的唯一理由是实测假红：agent 会用 `cat > f <<EOF` /
/// `printf ... > f` 写文件（三轮全量里 005/006/007 都是这么完成任务的），
/// 把 `must_call = ["write"]` 当契约就会把正确实现判成失败。
/// 反过来，`2>&1`、`> /dev/null` 这类纯吞输出的写法不能被当成「改了文件」，
/// 否则断言就失去证伪能力。
#[test]
fn must_modify_files_accepts_bash_redirection_but_not_output_discarding() {
    use eval::{TrajectoryExpect, bash_writes_file, check_trajectory};

    let expect = TrajectoryExpect {
        must_modify_files: true,
        ..Default::default()
    };

    // 工具路径：write / edit 都算
    for tool in ["write", "edit"] {
        let trace = eval::Trace {
            tool_calls: vec![tool.into()],
            ended_normally: true,
            ..Default::default()
        };
        assert!(
            check_trajectory(&expect, &trace).is_empty(),
            "{tool} 应算改动文件"
        );
    }

    // bash 重定向写真实文件：算
    for cmd in [
        "cat > src/a.rs <<'EOF'
x
EOF",
        "printf 'new\n' > README.md",
        "echo hi >> notes.txt",
        "sed s/a/b/ in.txt > out.txt",
    ] {
        let trace = eval::Trace {
            tool_calls: vec!["bash".into()],
            bash_commands: vec![cmd.into()],
            ended_normally: true,
            ..Default::default()
        };
        assert!(
            check_trajectory(&expect, &trace).is_empty(),
            "应识别为写文件：{cmd}"
        );
    }

    // 只读 + 吞输出：不算
    for cmd in [
        "ls -la 2>&1",
        "grep foo src/a.rs > /dev/null",
        "cat f | tee /dev/stderr | wc -l",
        "make > /dev/null 2>&1",
        "echo done",
        // 不带 -i 的 sed/perl 只输出到 stdout，不改文件
        "sed 's/a/b/' f.txt",
        "perl -pe 's/a/b/' f.txt",
    ] {
        assert!(!bash_writes_file(cmd), "不应判为写文件：{cmd}");
        let trace = eval::Trace {
            tool_calls: vec!["bash".into()],
            bash_commands: vec![cmd.into()],
            ended_normally: true,
            ..Default::default()
        };
        assert_eq!(
            check_trajectory(&expect, &trace).len(),
            1,
            "应报 must_modify_files 未满足：{cmd}"
        );
    }

    // 未开启该断言时绝不干扰
    let off = TrajectoryExpect::default();
    let trace = eval::Trace {
        tool_calls: vec!["bash".into()],
        bash_commands: vec!["ls".into()],
        ended_normally: true,
        ..Default::default()
    };
    assert!(check_trajectory(&off, &trace).is_empty());
}

/// 结果正则的锚点语义必须钉住，否则 case 会因一个不可见的换行而假红。
///
/// 实测踩到：case 029 的内容断言写成 `^new$`，而文件内容必然是 `"new\n"`
/// （写文件时几乎总会带尾换行）。regex crate 的 `$` 在非 multi-line 模式下
/// 只匹配字符串**末尾**，不匹配末尾换行之前，`^new$` 对 `"new\n"` 恒为 false。
/// 结果断言永远失败，而任务其实完成得完全正确。
#[test]
fn result_regex_dollar_does_not_match_before_trailing_newline() {
    let strict = regex::Regex::new(r"^new$").unwrap();
    assert!(
        !strict.is_match("new\n"),
        "`$` 不匹配尾换行前，这是假红的根因"
    );

    let trailing_ok = regex::Regex::new(r"^new\s*$").unwrap();
    assert!(trailing_ok.is_match("new\n"), "^new\\s*$ 应能接受尾换行");
    assert!(trailing_ok.is_match("new"), "^new\\s*$ 也应接受无尾换行");
}
