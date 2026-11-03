//! Golden-set eval 的 baseline 快照：检测**轨迹回归**。
//!
//! ## 为什么需要它
//!
//! 现有的 case 断言（`expect.trajectory` / `expect.result`）是**绝对阈值**：
//! `max_tool_calls = 7`、`max_input_tokens = 36000`。它能抓「彻底崩了」，
//! 抓不到「慢慢变差」——agent 从 3 步变成 6 步、token 从 8000 涨到 34000，
//! 只要没撞上限就一直是绿的。基线解决的是这个：跟**上一次已知好**的状态比。
//!
//! ## 设计上的关键取舍：不比对工具调用序列
//!
//! 最直觉的做法是「把 `tool_calls` 序列存下来，下次必须一模一样」。**这是错的**，
//! 依据是本仓库实测：这个 agent 写同一个文件会在 `write` / `edit` /
//! `cat > f <<EOF` / `printf > f` / `sed -i` / `perl -pi` 之间随机选择，
//! 四轮全量的轨迹各不相同，但结果断言全部通过、文件内容全部正确。
//! 严格比对序列会把「换了个同样正确的写法」判成回归 = 大量假红。
//! 假红比没有基线更糟：它会训练人 `--bless` 掉一切告警，机制直接失效。
//!
//! 所以基线分三层，按「可信度」从高到低：
//!
//! | 层 | 内容 | 判定 |
//! |---|---|---|
//! | L0 硬契约 | `ended_normally`、是否改过文件 | 必须一致 |
//! | L1 语义 | case 自己声明的结果正则**是否仍然匹配** | 必须全部匹配 |
//! | L2 预算 | `tool_calls` 总数、`input_tokens` | 允许上浮，超出 tolerance 才报 |
//! | L3 形状 | 工具名**去重集合**（用什么工具，而非顺序） | 只警告，不失败 |
//!
//! ## 为什么 L1 用「正则是否仍匹配」而不是「内容 hash 是否相同」
//!
//! 这是本模块最容易被写错的地方，代价已经实测过。第一版把结果文件内容
//! 存成 hash 做逐字节比对，结果**刚建完基线、紧接着跑一次就有 5/30 判红**。
//! 抓两条真实 diff：
//!
//! ```text
//! # case 006 同一 prompt 连跑两次，src/a.rs：
//! round1: pub const SECONDS_PER_DAY: u64 = 86400;\n\npub fn a() -> u64 { SECONDS_PER_DAY }\n
//! round2: /// Number of seconds in one day.\npub const SECONDS_PER_DAY: u64 = 86400;\n\npub fn a() -> u64 { SECONDS_PER_DAY }
//! ```
//!
//! 两次都完全正确：常量名对、值对、用法对。差异只有「有没有写文档注释」
//! 和「末尾有没有换行」——而这两项**都不是 case 要考察的东西**。
//! 这类任务是符号级改写，同一个需求有无数种等价写法（加不加注释、
//! `86400` 还是 `86_400`、常量放文件头还是文件尾）。把具体文本当契约，
//! 等于要求 LLM 每次产出逐字节相同的代码，这不可能，也没必要。
//!
//! 所以基线在 L1 层**只复用 case 已经声明的正则**（`expect.result.files`），
//! 它表达的正是「这条 case 真正在意什么」。基线额外提供的价值在 L2/L3：
//! 同一个正则今天还能匹配、但代价从 3 步涨到 9 步、token 涨一倍 —— 那是
//! 真实的退化，而 `max_tool_calls` 这类绝对上限要等到撞线才报。
//!
//! L0 是硬契约（该结束没结束、该改文件没改，一定是坏了）。L2 用「相对上浮
//! 比例」而不是绝对值，因为不同 case 的量级差 10 倍。L3 只观察——它正是
//! 「工具选错但碰巧写对了」的信号，但噪声太大（实测 30 条里 15 条都报形状
//! 变化，因为写文件手段本身在 write/edit/cat>/printf>/sed -i 之间随机），
//! 不足以单独判失败。
//!
//! ## 为什么 `--bless` 绝不能进 CI
//!
//! `--bless` 是「把当前状态当作正确答案」。在 CI 里跑 bless 等于自动把回归
//! 吞掉——每次 CI 都重写基线，基线永远等于最新代码，检测能力归零。
//! 判据：`justfile` 的 `eval` recipe 区分 `bless` 参数，`ci` 不调 `eval`
//! （它要真 API key）；而 `eval-baseline-check` 这个不需要 key 的单测进 `ci`，
//! 保证比对逻辑本身没被改坏。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::eval::Trace;

/// 一条 case 的基线快照。序列化成 `<case_id>.json` 存在 `baseline/` 下。
///
/// 字段全部 `pub` 且带 `serde` 默认值：老基线缺新字段时反序列化不报错，
/// 避免「加了个观察字段就让所有基线失效」。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Baseline {
    /// 写这份基线时的 case id，用于发现「文件放错位置」。
    pub id: String,
    /// L1：工具调用总数（不是序列）。
    pub tool_calls: usize,
    /// L1：累计 input token。
    pub input_tokens: u64,
    /// L1：是否正常结束。
    pub ended_normally: bool,
    /// L1：这条轨迹是否改动过文件。
    pub modified_files: bool,
    /// L1：结果文件 → **case 自己声明的正则**（`expect.result.files` 的原样拷贝）。
    ///
    /// 基线里存正则而不是内容 hash 的理由见模块注释（实测 5/30 假红）。
    /// 存正则还带来一个好处：case 的正则若被修改，基线与 case 会明确不一致，
    /// 提示「这条 case 的判定口径变了，该重新 bless」。
    ///
    /// 保留字段名 `result_hashes` 之外的新名字是为了让旧基线文件**显式失效**
    /// （反序列化时缺字段 → 默认空 map → 不参与 L1），而不是被静默误读。
    #[serde(default)]
    pub result_patterns: std::collections::BTreeMap<String, String>,
    /// L1：结果文件 → **归一化后**内容的 FNV-1a 64 位 hash（十六进制）。
    ///
    /// 「归一化」= 去掉行尾空白、把连续空行压成一个、去首尾空白。
    /// 为什么需要它（实测教训）：同一 case 连跑两次，`notes.md` 一次是
    /// `"hello"`、一次是 `"hello\n"`，两者都满足 prompt 的「exactly the
    /// single line: hello」，也都能通过 case 正则 `^hello\\s*$`。裸逐字节
    /// hash 会把这种**合法波动**判成回归 —— 引入基线第一天就假红。
    /// 归一化只保留「语义上不同的内容」，尾随换行/多余空格不再触发。
    ///
    /// 用 hash 而不是原文：原文存进 git 会产生大量无信息 diff。
    /// 用 FNV-1a 而不是引入 sha2：只防「内容变了」，不防对抗，不增加依赖。
    #[serde(default)]
    pub result_hashes: std::collections::BTreeMap<String, String>,
    /// L3：用过的工具名集合（去重、排序）。只警告不失败。
    #[serde(default)]
    pub tool_kinds: Vec<String>,
    /// 写基线时的时长（秒），仅作参考，不参与判定。
    #[serde(default)]
    pub secs: u64,
    /// 这条基线由几次采样得出（中位数）。1 = 单次采样，可靠度低。
    #[serde(default = "one")]
    pub samples: usize,
}

/// 比对结果。`failures` 非空即回归；`warnings` 只打印。
#[derive(Debug, Default)]
pub struct Verdict {
    pub failures: Vec<String>,
    pub warnings: Vec<String>,
}

impl Verdict {
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }
}

/// 工具调用数的上浮容忍度（百分比）与固定余量。
///
/// 两个参数**同时生效，取较大者**，因为单靠百分比在低步数 case 上太紧，
/// 单靠固定余量在高步数 case 上太松。实测依据：
///
/// - case 018（找 TODO）连跑 4 次：3 / 5 / 4 / 5 步。基线取到 3（最低点）时
///   +50% → 上限 5，后续 5 步勉强通过、6 步就红。
/// - case 026（不越界）连跑 7 次：3 / 3 / 3 / 5 / 4 / 4 / 7 步。中位数 3，
///   +50% → 上限 5，而 7 步是实际出现过的正常长尾（该轮耗时 27s，其它轮
///   15~21s，说明是模型侧慢而不是任务变复杂）。
///
/// 低步数 case（1~3 步）里「多跑一次 bash 确认」就是 +100%，用百分比判断
/// 必然假红；高步数 case（7~11 步）里 +50% 已有 3~5 步余量，够用。
/// 固定余量 4 步 + 百分比 50% 取大，能同时覆盖两端。
pub const TOLERANCE_PCT: u64 = 50;

/// 工具调用数的固定余量（步）。见 `TOLERANCE_PCT` 的实测依据。
///
/// 为什么是 4：低步数 case 的正常波动是「多跑 1~2 次确认」，加上离群到
/// +4 步仍属可接受；而真退化（任务理解跑偏、反复重试）通常 +6 步以上。
pub const CALL_SLACK: u64 = 4;

/// input token 的上浮容忍度（百分比）。
///
/// 为什么比步数宽松得多（实测依据）：token 不是独立变量，它**随步数累积**。
/// 每多一次工具往返就多一轮完整上下文（本仓库实测单轮往返约 5000 token）。
/// 基线 1 次调用（9829 token）的 case，多跑 1 步就必然到 15000（+52%），
/// 这不是退化，只是这次多看了一眼。
///
/// 实测反例（case 027）：基线 9829 token / 1 步，某次 14923 token / 2 步，
/// 用 50% 判红——而那条 case 的答案完全正确。假红会毁掉整个机制。
///
/// 取 150%：仍能抓住「token 翻两倍以上」的真失控（比如把整个大文件反复读
/// 进上下文），同时容忍「多一两次往返」这种正常波动。
/// 步数由 `TOLERANCE_PCT` 独立把关，两者互为交叉验证：真的绕远路会先在
/// 步数上暴露，token 只是滞后指标。
pub const TOKEN_TOLERANCE_PCT: u64 = 150;

fn one() -> usize {
    1
}

/// 计算内容的 FNV-1a 64 位 hash，返回十六进制字符串。
///
/// 选 FNV-1a 而非 std 的 DefaultHasher：后者**不保证跨 Rust 版本稳定**，
/// 用它存进 git 的基线会在升级工具链后全部失配，制造莫名其妙的红。
pub fn content_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// 归一化文件内容，去掉不影响语义的空白差异。
///
/// 做三件事：每行去行尾空白、连续空行压缩成一行、整体去首尾空行。
/// 不做「全部空白折叠成一个空格」那种激进归一化——那会把 `a b` 和 `a\nb`
/// 视作相同，掩盖真正的格式回归。
pub fn normalize_content(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut last_blank = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            if last_blank {
                continue;
            }
            last_blank = true;
        } else {
            last_blank = false;
        }
        out.push(trimmed);
    }
    while out.first().is_some_and(|l| l.is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// 从**多次采样**构造基线：步数与 token 取中位数。
///
/// 为什么必须多次采样：LLM 对同一任务用的步数有真实随机性。实测 case 018
/// 连跑 4 次得到 3 / 5 / 4 / 5 步；单次采样若取到 3（最低点），后续每次
/// 4~5 步都会被判回归。中位数（4）才是这个 case 的「典型代价」。
///
/// 中位数而非均值：步数分布右偏（偶尔一次绕远路拉到 12 步），均值会被
/// 单个离群值拉高，让基线变得过松、失去检测能力。
pub fn build_from_samples(id: &str, samples: &[(Trace, u64)]) -> Baseline {
    assert!(!samples.is_empty(), "至少需要一次采样");
    let mut calls: Vec<u64> = samples
        .iter()
        .map(|(t, _)| t.tool_calls.len() as u64)
        .collect();
    let mut toks: Vec<u64> = samples.iter().map(|(t, _)| t.input_tokens).collect();
    let mut secs: Vec<u64> = samples.iter().map(|(_, s)| *s).collect();
    calls.sort_unstable();
    toks.sort_unstable();
    secs.sort_unstable();
    let mid = samples.len() / 2;

    // 其余字段用「多数票」：ended_normally / modified_files 取任一为真即真
    // （只要有一次做对，就说明这个 case 是可达的），tool_kinds 取并集
    // （任一采样里用过的工具都算「这条 case 的正常手段」）。
    let ended = samples.iter().any(|(t, _)| t.ended_normally);
    let modified = samples.iter().any(|(t, _)| t.modified_files());
    let mut kinds: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (t, _) in samples {
        kinds.extend(t.tool_calls.iter().cloned());
    }

    Baseline {
        id: id.to_string(),
        tool_calls: calls[mid] as usize,
        input_tokens: toks[mid],
        ended_normally: ended,
        modified_files: modified,
        result_patterns: std::collections::BTreeMap::new(),
        result_hashes: std::collections::BTreeMap::new(),
        tool_kinds: kinds.into_iter().collect(),
        secs: secs[mid],
        samples: samples.len(),
    }
}

/// 从一次运行的轨迹与结果文件构造基线。
pub fn build(
    id: &str,
    trace: &Trace,
    result_patterns: &std::collections::BTreeMap<String, String>,
    secs: u64,
) -> Baseline {
    Baseline {
        id: id.to_string(),
        tool_calls: trace.tool_calls.len(),
        input_tokens: trace.input_tokens,
        ended_normally: trace.ended_normally,
        modified_files: trace.modified_files(),
        result_patterns: result_patterns.clone(),
        result_hashes: std::collections::BTreeMap::new(),
        tool_kinds: tool_kinds(trace),
        secs,
        samples: 1,
    }
}

/// 工具名集合（去重排序）。L3 用，也只用于观察。
pub fn tool_kinds(trace: &Trace) -> Vec<String> {
    trace
        .tool_calls
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// 允许的上浮上限：`base * (1 + TOLERANCE_PCT/100)`，加 1 避免 base=0 时恒为 0。
fn ceiling(base: u64, pct: u64) -> u64 {
    base + base * pct / 100 + 1
}

/// 比对一个 case 的实测结果与基线。
///
/// `actual_files` 是这条 case **声明要检查的**结果文件的实际内容
/// （键与 baseline 的 `result_hashes` 对齐）。只比基线里有的键——
/// 基线里没有的文件不参与判定，避免「后来新增的 case 字段」直接判红。
pub fn compare(
    baseline: &Baseline,
    trace: &Trace,
    actual_files: &std::collections::BTreeMap<String, String>,
) -> Verdict {
    let mut v = Verdict::default();

    // ---- L1：硬契约 ----
    if baseline.ended_normally && !trace.ended_normally {
        v.failures
            .push("回归：基线是正常结束，本次未正常结束（缺 Done/EndTurn）".into());
    }
    if baseline.modified_files && !trace.modified_files() {
        v.failures
            .push("回归：基线改动过文件，本次一个文件都没改（工具调用与实际不符）".into());
    }
    for (rel, pattern) in &baseline.result_patterns {
        match actual_files.get(rel) {
            Some(content) => match regex::Regex::new(pattern) {
                Ok(re) => {
                    if !re.is_match(content) {
                        v.failures.push(format!(
                            "回归：{rel} 不再匹配 case 声明的 /{pattern}/（当初能匹配）"
                        ));
                    }
                }
                Err(e) => v
                    .failures
                    .push(format!("基线里 {rel} 的正则 /{pattern}/ 非法：{e}")),
            },
            None => v.failures.push(format!(
                "回归：{rel} 在基线里存在，本次该 case 没有产出/未检查该文件"
            )),
        }
    }

    // ---- L2：预算上浮 ----
    let calls = trace.tool_calls.len() as u64;
    let call_base = baseline.tool_calls as u64;
    let call_max = ceiling(call_base, TOLERANCE_PCT).max(call_base + CALL_SLACK);
    if calls > call_max {
        v.failures.push(format!(
            "回归：工具调用 {} 次，基线 {} 次（上限 {call_max} = 基线 +{}% 或 +{CALL_SLACK} 步取大）",
            calls, baseline.tool_calls, TOLERANCE_PCT
        ));
    }
    let tok_max = ceiling(baseline.input_tokens, TOKEN_TOLERANCE_PCT);
    if trace.input_tokens > tok_max {
        v.failures.push(format!(
            "回归：input token {}，基线 {}（上限 {tok_max}，容忍 +{TOKEN_TOLERANCE_PCT}%）",
            trace.input_tokens, baseline.input_tokens
        ));
    }

    // ---- L3：形状（只警告）----
    let now = tool_kinds(trace);
    if now != baseline.tool_kinds {
        let added: Vec<_> = now
            .iter()
            .filter(|k| !baseline.tool_kinds.contains(k))
            .cloned()
            .collect();
        let dropped: Vec<_> = baseline
            .tool_kinds
            .iter()
            .filter(|k| !now.contains(k))
            .cloned()
            .collect();
        v.warnings.push(format!(
            "形状变化（不判失败）：新增工具 {added:?}，不再使用 {dropped:?}"
        ));
    }

    v
}

/// 基线目录（`tests/eval/baseline/`）。
pub fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/eval/baseline")
}

/// 某条 case 的基线文件路径。
pub fn path_for(case_id: &str) -> PathBuf {
    dir().join(format!("{case_id}.json"))
}

/// 读基线。不存在返回 `Ok(None)`——首次引入基线时所有 case 都还没有。
pub fn load(case_id: &str) -> Result<Option<Baseline>, String> {
    let p = path_for(case_id);
    match std::fs::read_to_string(&p) {
        Ok(raw) => {
            let b: Baseline = serde_json::from_str(&raw)
                .map_err(|e| format!("基线 {} 解析失败：{e}", p.display()))?;
            if b.id != case_id {
                return Err(format!(
                    "基线 {} 里的 id 是 {}，与文件名不符（放错位置了？）",
                    p.display(),
                    b.id
                ));
            }
            Ok(Some(b))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("读基线 {} 失败：{e}", p.display())),
    }
}

/// 写基线（`--bless` 用）。
pub fn save(b: &Baseline) -> Result<PathBuf, String> {
    let p = path_for(&b.id);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败：{e}"))?;
    }
    // 稳定键序 + 末尾换行：让 git diff 只反映真实变化
    let mut json = serde_json::to_string_pretty(b).map_err(|e| format!("序列化基线失败：{e}"))?;
    json.push('\n');
    std::fs::write(&p, json).map_err(|e| format!("写基线 {} 失败：{e}", p.display()))?;
    Ok(p)
}

/// 是否处于 `--bless` 模式。
///
/// 判据用环境变量 `EVAL_BLESS`（justfile 传），不用命令行参数：
/// `cargo test` 自己解析 `--` 之后的参数，多塞自定义 flag 会让 cargo 报错。
pub fn bless_mode() -> bool {
    matches!(
        std::env::var("EVAL_BLESS").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// 统计一个目录下的基线文件数，供 runner 报告覆盖率
/// （「30 条 case 里有几条有基线」）。
pub fn count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
                .count()
        })
        .unwrap_or(0)
}
