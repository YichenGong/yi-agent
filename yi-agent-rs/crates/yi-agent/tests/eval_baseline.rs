//! Golden-set eval 的驱动入口。
//!
//! 全部 `#[ignore]`：真钱（真实 API 调用）+ 分钟级耗时，不进 `just ci`。
//!
//! ## 两种模式
//!
//! ```
//! just eval              # 比对模式：跑 case，并与 baseline/ 下的快照比
//! just eval bless=1      # 写基线：把本次结果固化为新的 baseline
//! ```
//!
//! 首次引入基线时 baseline/ 是空的，比对模式会对**没有基线**的 case 打印
//! 「无基线，跳过比对」而不是判失败——否则第一次跑就全红，逼人无脑 bless。

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
    let bless = eval::baseline::bless_mode();
    let cases = eval::load_cases(&cases_dir());
    assert!(!cases.is_empty(), "cases 目录里没有 case");

    // bless 默认采 3 次取中位数：单次采样会被 LLM 随机性带偏（实测 case 018
    // 连跑 4 次是 3/5/4/5 步，单次取到 3 会让后续每次都判红）。
    // 可用 EVAL_SAMPLES 覆盖，1 = 单次（快但不可靠）。
    let samples: usize = std::env::var("EVAL_SAMPLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(3);

    if bless {
        println!(
            "MODE bless —— 每条 case 采样 {samples} 次取中位数，写入 {}",
            eval::baseline::dir().display()
        );
    }

    if !bless {
        let have = eval::baseline::count(&eval::baseline::dir());
        println!(
            "MODE compare —— 已有基线 {have} 条 / case {} 条",
            cases.len()
        );
    }

    let bin = common::yi_agent_bin();
    // 两类失败必须分开统计，否则「基线回归」的信噪比会被 case 自身的
    // 能力失败淹没：
    //   - case_fails：case 自己的断言（结果正则、must_modify_files…）没过。
    //     含义是「agent 这次没完成这件事」——与基线无关，是真实能力信号。
    //   - regressions：case 断言全过，但比基线更费（步数/token 超容忍）。
    //     含义是「仍然做对了，但代价变高」。
    let mut case_fails = Vec::new();
    let mut regressions = Vec::new();
    let mut ran = 0usize;
    let mut blessed = 0usize;
    let mut compared = 0usize;
    let mut no_baseline = Vec::new();

    for (path, case) in &cases {
        if !filter.is_empty() && !case.id.contains(&filter) {
            continue;
        }
        ran += 1;
        let started = std::time::Instant::now();
        let out = eval::run_case(&bin, case);
        let secs = started.elapsed().as_secs();

        // 先收集轨迹（失败时用 last_trace 兜底，便于校准预算）
        let trace = match &out {
            Ok(o) => Some(o.trace.clone()),
            Err(_) => eval::last_trace(),
        };

        // 再判定本条结果
        let mut detail = String::new();
        let mut label = "PASS";
        let mut is_regression = false;
        match &out {
            Err(msg) => {
                label = "FAIL";
                detail = msg.clone();
            }
            Ok(o) => {
                if bless {
                    // 第一条采样已拿到（上面的 run_case），再补 samples-1 次
                    let mut taken: Vec<(eval::Trace, u64)> = vec![(o.trace.clone(), secs)];
                    for n in 2..=samples {
                        match eval::run_case(&bin, case) {
                            Ok(more) => {
                                println!(
                                    "  SAMPLE {}/{} {} calls={} tokens={}",
                                    n,
                                    samples,
                                    case.id,
                                    more.trace.tool_calls.len(),
                                    more.trace.input_tokens
                                );
                                taken.push((more.trace, started.elapsed().as_secs()));
                            }
                            Err(e) => {
                                println!("  SAMPLE {n} {} 失败（忽略该次）：{e}", case.id)
                            }
                        }
                    }
                    let mut b = eval::baseline::build_from_samples(&case.id, &taken);
                    b.result_patterns = case.expect.result.files.clone();
                    match eval::baseline::save(&b) {
                        Ok(p) => {
                            blessed += 1;
                            println!("BLESS {} -> {}", case.id, p.display());
                        }
                        Err(e) => {
                            label = "FAIL";
                            detail = format!("case {} 写基线失败：{e}", case.id);
                        }
                    }
                } else {
                    match eval::baseline::load(&case.id) {
                        Ok(Some(base)) => {
                            compared += 1;
                            let v = eval::baseline::compare(&base, &o.trace, &o.result_files);
                            for w in &v.warnings {
                                println!("  WARN {} {w}", case.id);
                            }
                            if !v.is_ok() {
                                label = "REGRESS";
                                is_regression = true;
                                detail = format!(
                                    "case {} 相对基线回归：\n  {}",
                                    case.id,
                                    v.failures.join("\n  ")
                                );
                            }
                        }
                        Ok(None) => no_baseline.push(case.id.clone()),
                        Err(e) => {
                            label = "FAIL";
                            detail = format!("case {} 读基线失败：{e}", case.id);
                        }
                    }
                }
            }
        }

        // 无论成败都打印实测轨迹：max_input_tokens / max_tool_calls 必须来自实测
        // （见 Spec C4），没有这行就只能靠猜，门禁要么永不触发要么频繁假失败。
        // 机器可读，便于把一轮输出直接转成 case 的预算。
        if let Some(t) = &trace {
            println!(
                "TRACE {} {} tools={:?} tokens={} secs={}",
                label, case.id, t.tool_calls, t.input_tokens, secs
            );
            // bash 命令要打出来，否则「用了 bash 但没调 edit」这类轨迹失败只能靠猜：
            // 是 agent 用 `sed -i` 绕过工具（真缺陷），还是它把 `cat` 当 `read`
            // 用（提示词问题）。这两种处置完全不同，不能混为一谈。
            if !t.bash_commands.is_empty() {
                println!("  BASH {:?}", t.bash_commands);
            }
        } else {
            println!("TRACE {} {} tools=<none> secs={}", label, case.id, secs);
        }
        if !detail.is_empty() {
            println!("{label} {}  (from {})", case.id, path.display());
            if is_regression {
                regressions.push(detail);
            } else {
                case_fails.push(detail);
            }
        }
    }

    println!("\n跑了 {ran} 条 case；bless {blessed} 条，比对 {compared} 条",);
    println!(
        "  case 自身断言失败 {} 条（agent 没完成，与基线无关）",
        case_fails.len()
    );
    println!("  相对基线回归 {} 条", regressions.len());
    if !no_baseline.is_empty() {
        println!(
            "\n注意：{} 条 case 没有基线，未做回归比对：{:?}",
            no_baseline.len(),
            no_baseline
        );
        println!("  跑 `just eval bless=1` 建立基线（首次引入基线时必须先做一次）。");
    }
    let mut all = case_fails.clone();
    all.extend(regressions.clone());
    assert!(all.is_empty(), "失败详情：\n\n{}", all.join("\n\n"));
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

// ---------------------------------------------------------------------------
// baseline 逻辑自检（不需要 API key，默认就跑，进 `just test`）
// ---------------------------------------------------------------------------

fn trace_of(tools: &[&str], tokens: u64, ended: bool) -> eval::Trace {
    eval::Trace {
        tool_calls: tools.iter().map(|s| s.to_string()).collect(),
        input_tokens: tokens,
        ended_normally: ended,
        bash_commands: Vec::new(),
    }
}

fn files(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn content_hash_is_stable_and_discriminating() {
    // 固定值：FNV-1a 必须跨进程/跨平台稳定，否则基线会在换机器后全部失配
    assert_eq!(eval::baseline::content_hash(b""), "cbf29ce484222325");
    assert_eq!(eval::baseline::content_hash(b"a"), "af63dc4c8601ec8c");
    // 单字节差异必须改变 hash（否则「内容变了」检测不到）
    assert_ne!(
        eval::baseline::content_hash(b"hello"),
        eval::baseline::content_hash(b"hellp")
    );
    // 顺序敏感：`ab` 与 `ba` 不能撞
    assert_ne!(
        eval::baseline::content_hash(b"ab"),
        eval::baseline::content_hash(b"ba")
    );
}

#[test]
fn identical_run_has_no_failures() {
    let t = trace_of(&["read", "write"], 1000, true);
    let f = files(&[("notes.md", "hello\n")]);
    let b = eval::baseline::build("x", &t, &f, 3);
    let v = eval::baseline::compare(&b, &t, &f);
    assert!(v.is_ok(), "同样的轨迹与结果不应报回归：{:?}", v.failures);
    assert!(v.warnings.is_empty(), "形状一致时不应有警告");
}

#[test]
fn different_tool_order_is_not_a_regression() {
    // 这是本设计的核心断言：工具**顺序**变化不算回归。
    // 依据：实测同一 case 的写文件手段在 write/edit/cat>/printf>/sed -i 之间随机，
    // 严格比对序列会大量假红。这里改为「同类工具换个顺序」必须通过。
    let baseline_trace = trace_of(&["read", "bash"], 1000, true);
    let f = files(&[]);
    let b = eval::baseline::build("x", &baseline_trace, &f, 3);
    let actual = trace_of(&["bash", "read"], 1000, true);
    let v = eval::baseline::compare(&b, &actual, &f);
    assert!(v.is_ok(), "顺序不同但工具集合相同，不应判回归");
}

#[test]
fn semantic_mismatch_is_a_regression() {
    // L1 的口径：case 声明的正则**当初能匹配、现在不能** → 回归。
    // 不用「内容 hash 变了」——那会把等价写法（加注释、尾随换行）判成回归，
    // 实测刚建完基线就跑出 5/30 假红（见模块注释）。
    let t = trace_of(&["write"], 900, true);
    let pat = files(&[("a.txt", "^hello$")]);
    let b = eval::baseline::build("x", &t, &pat, 2);
    // 仍匹配 → 通过
    let v = eval::baseline::compare(&b, &t, &files(&[("a.txt", "hello")]));
    assert!(v.is_ok(), "仍匹配声明正则应通过：{:?}", v.failures);
    // 不再匹配 → 回归
    let v2 = eval::baseline::compare(&b, &t, &files(&[("a.txt", "bye")]));
    assert!(!v2.is_ok(), "不匹配声明正则必须判回归");
    assert!(v2.failures.iter().any(|m| m.contains("不再匹配")));
}

#[test]
fn equivalent_rewrites_are_not_regressions() {
    // 核心断言：同一需求的等价写法不算回归。
    // 依据是实测——case 006 连跑两次，一次带文档注释、一次不带，都对。
    let t = trace_of(&["write"], 900, true);
    let pat = files(&[("a.rs", "SECONDS_PER_DAY")]);
    let b = eval::baseline::build("x", &t, &pat, 2);
    for variant in [
        "pub const SECONDS_PER_DAY: u64 = 86400;\n\npub fn a() -> u64 { SECONDS_PER_DAY }\n",
        "/// Number of seconds in one day.\npub const SECONDS_PER_DAY: u64 = 86400;\n\npub fn a() -> u64 { SECONDS_PER_DAY }",
        "pub const SECONDS_PER_DAY: u64 = 86_400;\npub fn a() -> u64 { SECONDS_PER_DAY }\n",
    ] {
        let v = eval::baseline::compare(&b, &t, &files(&[("a.rs", variant)]));
        assert!(v.is_ok(), "等价写法不应判回归：{:?}", v.failures);
    }
}

#[test]
fn missing_result_file_is_a_regression() {
    let t = trace_of(&["write"], 900, true);
    let pat = files(&[("a.txt", "hello")]);
    let b = eval::baseline::build("x", &t, &pat, 2);
    // 本次该文件没被创建 → result_files 里没有这个键
    let v = eval::baseline::compare(&b, &t, &files(&[]));
    assert!(!v.is_ok(), "基线里有的文件本次没产出，必须判回归");
    assert!(v.failures.iter().any(|m| m.contains("未检查该文件")));
}

#[test]
fn ended_normally_loss_is_a_regression() {
    let b = eval::baseline::build("x", &trace_of(&["read"], 100, true), &files(&[]), 1);
    let v = eval::baseline::compare(&b, &trace_of(&["read"], 100, false), &files(&[]));
    assert!(!v.is_ok(), "基线正常结束、本次未结束，必须判回归");
}

#[test]
fn modified_files_loss_is_a_regression() {
    let b = eval::baseline::build(
        "x",
        &trace_of(&["write"], 100, true),
        &files(&[("a", "1")]),
        1,
    );
    // 只读、一个文件都没改
    let v = eval::baseline::compare(&b, &trace_of(&["read"], 100, true), &files(&[("a", "1")]));
    assert!(!v.is_ok(), "基线改过文件、本次没改，必须判回归");
}

#[test]
fn budget_drift_within_tolerance_passes() {
    let b = eval::baseline::build(
        "x",
        &trace_of(&["read", "write"], 1000, true),
        &files(&[]),
        1,
    );
    // +40% token，仍在 50% 容忍内
    let v = eval::baseline::compare(&b, &trace_of(&["read", "write"], 1400, true), &files(&[]));
    assert!(v.is_ok(), "40% 上浮在容忍内，不应报回归：{:?}", v.failures);
}

#[test]
fn budget_drift_beyond_tolerance_fails() {
    let b = eval::baseline::build(
        "x",
        &trace_of(&["read", "write"], 1000, true),
        &files(&[]),
        1,
    );
    // token 容忍 150% → 上限 2501。给 5000（5 倍）必须判回归。
    let v = eval::baseline::compare(&b, &trace_of(&["read", "write"], 5000, true), &files(&[]));
    assert!(!v.is_ok(), "token 涨到 5 倍必须判回归");
    assert!(v.failures.iter().any(|m| m.contains("input token")));
}

#[test]
fn one_extra_round_trip_does_not_fail_on_tokens() {
    // 实测反例（case 027）：基线 9829 token/1 步，某次 14923 token/2 步，
    // 答案完全正确。token 容忍度必须能容纳「多一次往返」。
    let b = eval::baseline::build("x", &trace_of(&["read"], 9829, true), &files(&[]), 1);
    let v = eval::baseline::compare(&b, &trace_of(&["read", "bash"], 14923, true), &files(&[]));
    assert!(
        v.is_ok(),
        "多一次往返（+52% token）不应判回归：{:?}",
        v.failures
    );
}

#[test]
fn more_tool_calls_beyond_tolerance_fails() {
    // 基线 2 次：百分比上限 = 2+1+1 = 4；固定余量上限 = 2+4 = 6，取大 6。
    // 故实测 7 次才判红。
    let b = eval::baseline::build(
        "x",
        &trace_of(&["read", "write"], 100, true),
        &files(&[]),
        1,
    );
    // 注意必须含 write：基线 modified_files=true，纯 read 轨迹会先撞
    // modified_files 回归，测不到步数判定。
    let many = trace_of(
        &["read", "write", "read", "read", "read", "read", "read"],
        100,
        true,
    );
    let v = eval::baseline::compare(&b, &many, &files(&[]));
    assert!(!v.is_ok(), "调用数 7 超过上限 6，必须判回归");
    assert!(v.failures.iter().any(|m| m.contains("工具调用")));
    // 6 次（恰在上限）不应红
    let ok6 = trace_of(
        &["read", "write", "read", "read", "read", "read"],
        100,
        true,
    );
    assert!(
        eval::baseline::compare(&b, &ok6, &files(&[])).is_ok(),
        "6 次等于上限，不应判回归：{:?}",
        eval::baseline::compare(&b, &ok6, &files(&[])).failures
    );
}

#[test]
fn tool_kind_change_warns_but_does_not_fail() {
    // 形状变化（read→bash 替代）只警告。这是「工具选错但碰巧写对」的信号，
    // 但噪声大，单独不足以判失败。
    let b = eval::baseline::build("x", &trace_of(&["read"], 100, true), &files(&[]), 1);
    let v = eval::baseline::compare(&b, &trace_of(&["bash"], 100, true), &files(&[]));
    assert!(v.is_ok(), "形状变化不应判失败");
    assert_eq!(v.warnings.len(), 1, "应产生一条形状警告");
    assert!(v.warnings[0].contains("新增工具"));
}

#[test]
fn missing_baseline_is_reported_not_failed() {
    // 首次引入基线时 baseline/ 为空，必须返回 Ok(None) 而不是 Err
    let r = eval::baseline::load("999-does-not-exist");
    assert!(matches!(r, Ok(None)), "不存在的基线应返回 Ok(None)：{r:?}");
}

#[test]
fn bless_mode_reads_env() {
    // 这个测试改全局环境变量，用独立名字避免与其它测试竞争
    unsafe { std::env::set_var("EVAL_BLESS", "1") };
    assert!(eval::baseline::bless_mode());
    unsafe { std::env::set_var("EVAL_BLESS", "0") };
    assert!(!eval::baseline::bless_mode());
    unsafe { std::env::remove_var("EVAL_BLESS") };
    assert!(!eval::baseline::bless_mode());
}

#[test]
fn trailing_newline_is_not_a_regression() {
    // 实测教训：同一 case 连跑两次，notes.md 一次 "hello" 一次 "hello\n"，
    // 都满足 prompt 与 case 正则（`^hello\\s*$`）。
    let t = trace_of(&["write"], 900, true);
    let pat = files(&[("a.txt", "^hello\\s*$")]);
    let b = eval::baseline::build("x", &t, &pat, 2);
    let v = eval::baseline::compare(&b, &t, &files(&[("a.txt", "hello\n")]));
    assert!(v.is_ok(), "尾随换行不应判回归：{:?}", v.failures);
}

#[test]
fn trailing_whitespace_differences_are_not_regressions() {
    let t = trace_of(&["write"], 900, true);
    let pat = files(&[("a.txt", "a[\\s\\S]*b")]);
    let b = eval::baseline::build("x", &t, &pat, 2);
    for variant in ["a\nb", "a  \nb\t\n", "a\n\nb\n\n\n", "a\nb\n"] {
        let v = eval::baseline::compare(&b, &t, &files(&[("a.txt", variant)]));
        assert!(v.is_ok(), "变体 {variant:?} 不应判回归：{:?}", v.failures);
    }
}

#[test]
fn regex_is_still_authoritative() {
    // 基线不能比 case 本身更宽松：case 要求 "x + 1"，实测是 "x + 2" 必须红。
    let t = trace_of(&["write"], 900, true);
    let pat = files(&[("a.rs", "x \\+ 1")]);
    let b = eval::baseline::build("x", &t, &pat, 2);
    let v = eval::baseline::compare(&b, &t, &files(&[("a.rs", "x + 2")]));
    assert!(!v.is_ok(), "不满足 case 正则必须判回归");
}

#[test]
fn normalize_content_keeps_single_blank_line() {
    assert_eq!(eval::baseline::normalize_content("a\n\n\nb"), "a\n\nb");
    assert_eq!(eval::baseline::normalize_content("\n\na\n\n"), "a");
    assert_eq!(eval::baseline::normalize_content(""), "");
    assert_eq!(eval::baseline::normalize_content("   \n\t\n"), "");
}

#[test]
fn median_sampling_absorbs_step_variance() {
    // 实测数据：case 018 连跑 4 次得到 3 / 5 / 4 / 5 步。
    // 单次采样若取到 3，后续 5 步会判回归；中位数取 4~5 则全部通过。
    let samples: Vec<(eval::Trace, u64)> = [3usize, 5, 4, 5]
        .iter()
        .map(|n| {
            let tools: Vec<&str> = std::iter::repeat_n("bash", *n).collect();
            (trace_of(&tools, 20000, true), 10)
        })
        .collect();
    let b = eval::baseline::build_from_samples("x", &samples);
    // 排序后 3,4,5,5，偶数个取 index = len/2 = 2 → 5（偏上中位，
    // 刻意不取偏下的 4：基线偏高更保守，宁可漏报轻微变慢，也不轻易假红）
    assert_eq!(b.tool_calls, 5);
    assert_eq!(b.samples, 4);
    // 实测 5 步与基线一致，必须通过
    let five: Vec<&str> = std::iter::repeat_n("bash", 5).collect();
    let v = eval::baseline::compare(&b, &trace_of(&five, 20000, true), &files(&[]));
    assert!(v.is_ok(), "5 步相对基线 5 不应判回归：{:?}", v.failures);
}

#[test]
fn low_step_case_tolerates_extra_round_trip() {
    // 实测 case 026：7 次采样 3/3/3/5/4/4/7 步，中位数 3。
    // 百分比 50% 只给到上限 5，会漏掉真实出现过的 7 步；固定余量 4 给到 7，
    // 恰好覆盖。这条钉住「低步数不能用百分比判断」。
    let b = eval::baseline::build("x", &trace_of(&["bash"; 3], 21000, true), &files(&[]), 16);
    let seven = trace_of(&["bash"; 7], 40464, true);
    assert!(
        eval::baseline::compare(&b, &seven, &files(&[])).is_ok(),
        "低步数 case 的 7 步（实测出现过）不应判回归"
    );
    let eight = trace_of(&["bash"; 8], 40464, true);
    assert!(
        !eval::baseline::compare(&b, &eight, &files(&[])).is_ok(),
        "8 步超出余量，必须判回归"
    );
}

#[test]
fn median_uses_odd_middle_element() {
    // 3 次采样 2/9/4 → 排序 2,4,9 → 中位 4（不是均值 5）
    let samples: Vec<(eval::Trace, u64)> = [2usize, 9, 4]
        .iter()
        .map(|n| {
            let tools: Vec<&str> = std::iter::repeat_n("bash", *n).collect();
            (trace_of(&tools, 100, true), 1)
        })
        .collect();
    let b = eval::baseline::build_from_samples("x", &samples);
    assert_eq!(b.tool_calls, 4, "中位数应为 4，等距均值 5 会被离群值拉偏");
}

#[test]
fn median_sampling_unions_tool_kinds() {
    // 任一采样用过的工具都算正常手段，避免形状警告因采样不同而抖动
    let s1 = eval::Trace {
        tool_calls: vec!["write".into()],
        input_tokens: 100,
        ended_normally: true,
        bash_commands: vec![],
    };
    let s2 = eval::Trace {
        tool_calls: vec!["edit".into(), "bash".into()],
        input_tokens: 100,
        ended_normally: true,
        bash_commands: vec![],
    };
    let b = eval::baseline::build_from_samples("x", &[(s1, 1), (s2, 1)]);
    assert_eq!(b.tool_kinds, vec!["bash", "edit", "write"]);
}
