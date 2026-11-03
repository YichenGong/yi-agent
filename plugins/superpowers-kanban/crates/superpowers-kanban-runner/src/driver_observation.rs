//! `driver/next` 请求里观测事实的严格解析。
//!
//! 宿主每 tick 送来的参数里有两类事实：旧的 `threads[]`（哪些会话这一拍还被
//! driver 登记着、此刻闲不闲）和可选的新 `loaded_threads[]`（当前宿主热会话
//! 表的**完整**快照）。后者是对前者的补充而不是替代：字段缺失表示宿主没有
//! 完整快照能力，数组出现即声明完整，空数组表示热表确实为空。
//!
//! 这里只做解析，不做归一化：把 `threads[]` 原样落成既有调度事实、把显式
//! `alive` 单独记下来、把 `loaded_threads[]` 严格校验成 `LoadedThreadFacts`。
//! 快照与旧子集的优先级、与看板绑定 ID 的匹配留给后续的归一化步骤，避免在
//! 解析层就掺进看板语义。

use std::collections::BTreeMap;

use serde_json::Value;

/// 宿主热会话表里的一条事实。
///
/// 只有 ID 与「此刻是否空闲」，不携带消息、路径、标题或权限：宿主对看板没有
/// 任何归属信息，插件也不该从这份快照里推断某会话属于哪个插件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedThreadFacts {
    /// 宿主内存里的会话 ID，**原样**保存（不 trim、不折叠大小写）。
    pub thread_id: String,
    /// 与既有口径一致：`ThreadStatus::Idle` 且没有 active turn。
    pub idle: bool,
}

/// 一次 `driver/next` 请求解析出来的观测输入。
///
/// 三类事实刻意分开存：`legacy` 是既有调度口径（含保守默认值），
/// `reported_alive` 只装宿主**显式**给出的 `alive`，`loaded` 则是完整热表
/// 快照（`None` = 旧宿主没有这份能力）。把默认值混进报告会让「缺字段」
/// 冒充成「宿主说它还活着」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationInput {
    /// 旧 `threads[]`：缺 `idle` 当不闲、缺 `alive` 当活着（与今天一致）。
    pub legacy: Vec<crate::service::ThreadFacts>,
    /// 只记录 `alive` 键确实是 JSON boolean 的条目，false 也记。
    pub reported_alive: BTreeMap<String, bool>,
    /// `None` 表示宿主没发 `loaded_threads`（旧宿主）；`Some([])` 表示热表为空。
    pub loaded: Option<Vec<LoadedThreadFacts>>,
}

/// 解析 `driver/next` 的请求参数。
///
/// `threads` 缺失或形状不对、`loaded_threads` 显式给出但不合法，都是调用方的
/// 错：必须报错而不是静默当成空一拍——把无效快照降级成空数组，正好就是
/// 「这一拍所有会话都没加载」的误报来源。错误消息只带字段名与条目序号，不回显
/// 任何会话内容。
pub fn parse_observation_input(params: &Value) -> Result<ObservationInput, String> {
    let threads = params
        .get("threads")
        .and_then(Value::as_array)
        .ok_or_else(|| "driver/next needs a `threads` array".to_string())?;

    let mut legacy = Vec::with_capacity(threads.len());
    let mut reported_alive = BTreeMap::new();
    for thread in threads {
        let thread_id = thread
            .get("thread_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "driver/next threads need a `thread_id`".to_string())?;
        // 缺 `idle` 当作「不闲」：把忙当闲会让插件对一台正在跑的会话下结论。
        let idle = thread.get("idle").and_then(Value::as_bool).unwrap_or(false);
        // 缺 `alive` 当作「还活着」：保守方向相反——宁可不判 stale，
        // 也不凭一次缺字段就报废一张卡的观测（旧宿主不发这个键）。
        let alive = thread.get("alive").and_then(Value::as_bool).unwrap_or(true);
        // 只有显式 boolean 才是宿主的报告；上面的默认 `true` 不算。
        if let Some(alive) = thread.get("alive").and_then(Value::as_bool) {
            reported_alive.insert(thread_id.to_string(), alive);
        }
        legacy.push(crate::service::ThreadFacts {
            thread_id: thread_id.to_string(),
            idle,
            alive,
        });
    }

    let loaded = match params.get("loaded_threads") {
        None => None,
        Some(value) => {
            let array = value
                .as_array()
                .ok_or_else(|| "driver/next loaded_threads must be an array".to_string())?;
            let mut seen = std::collections::BTreeSet::new();
            let mut out = Vec::with_capacity(array.len());
            for (index, item) in array.iter().enumerate() {
                let id = item
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .ok_or_else(|| {
                        format!("driver/next loaded_threads[{index}] needs a nonblank thread_id")
                    })?;
                let idle = item.get("idle").and_then(Value::as_bool).ok_or_else(|| {
                    format!("driver/next loaded_threads[{index}] needs boolean idle")
                })?;
                if !seen.insert(id.to_owned()) {
                    return Err(format!(
                        "driver/next loaded_threads[{index}] duplicates thread_id"
                    ));
                }
                out.push(LoadedThreadFacts {
                    thread_id: id.to_owned(),
                    idle,
                });
            }
            Some(out)
        }
    };

    Ok(ObservationInput {
        legacy,
        reported_alive,
        loaded,
    })
}

/// 一次归一化的结果：既给调度用的有效事实，也给展示用的旁路文件。
///
/// 两者必须来自**同一份**判断，否则会出现「UI 说已加载、租约还以为死了」
/// 这种自相矛盾的拍。
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedObservation {
    /// 本拍有效事实（只覆盖相关 ID），供 gate 结算、租约对账、调度共用。
    pub facts: Vec<crate::service::ThreadFacts>,
    /// 要原子落盘的旁路：只含相关 ID，已剪掉无关旧记录。
    pub file: crate::observation::ObservationFile,
}

/// 把一份请求归一化成「本拍有效事实 + 新的观测旁路」。
///
/// `relevant` 是**唯一**的持久化范围：看板里绑定的非空执行 ID、仍有效的
/// merge attempt 绑定、以及匹配本进程 pending 的成功回报 ID。它由调用方在
/// 读取 board/attempt 之后给出，本函数不碰磁盘。
///
/// `previous` 是**只读**输入：返回的 `file` 是全新对象，调用方随后原子保存。
/// 三个分支：
///
/// - `loaded = Some(_)`：宿主给了完整快照，它对本拍是权威的。命中即
///   `alive=true` + 快照里的 `idle`；未命中即 `alive=false`、`idle=false`
///   （未加载不证明工具已经停止）。
/// - `loaded = None`（旧宿主）：调度沿用旧 `threads[]` 的 alive/idle；
///   但只有**显式** boolean `alive` 才构成展示证据，缺字段不写展示态。
/// - 相关 ID 这一拍完全没被提到：有旧记录就按旧规则记 `alive=false` 并保留
///   最后已加载时间，展示降级 unknown；从来没有记录就不插入（缺记录保持
///   既有的租约默认规则）。
pub fn normalize(
    input: &ObservationInput,
    relevant: &std::collections::BTreeSet<String>,
    previous: &crate::observation::ObservationFile,
    now: chrono::DateTime<chrono::Local>,
) -> NormalizedObservation {
    use crate::observation::{Observation, ObservationFile, SessionObservation};

    let mut facts = Vec::new();
    let mut file = ObservationFile::default();

    // 相关集合自带 BTree 顺序，产物因此稳定、可断言。
    for thread_id in relevant {
        let previous_entry = previous.threads.get(thread_id);
        let entry = match &input.loaded {
            // 完整快照：权威。
            Some(loaded) => match loaded.iter().find(|fact| &fact.thread_id == thread_id) {
                Some(fact) => {
                    facts.push(crate::service::ThreadFacts {
                        thread_id: thread_id.clone(),
                        idle: fact.idle,
                        alive: true,
                    });
                    Observation {
                        alive: true,
                        last_seen_alive_at: Some(now),
                        session_observation: Some(SessionObservation::Loaded.as_str().to_string()),
                        observed_at: Some(now.to_rfc3339()),
                    }
                }
                None => {
                    facts.push(crate::service::ThreadFacts {
                        thread_id: thread_id.clone(),
                        idle: false,
                        alive: false,
                    });
                    Observation {
                        alive: false,
                        // 未加载不证明它此前没被加载过，所以只沿用历史时间。
                        last_seen_alive_at: previous_entry.and_then(|old| old.last_seen_alive_at),
                        session_observation: Some(
                            SessionObservation::NotLoaded.as_str().to_string(),
                        ),
                        observed_at: Some(now.to_rfc3339()),
                    }
                }
            },
            // 旧宿主：调度默认值照旧，但展示只认显式报告。
            None => {
                let legacy = input
                    .legacy
                    .iter()
                    .find(|fact| &fact.thread_id == thread_id);
                let explicit = input.reported_alive.get(thread_id).copied();
                match legacy {
                    Some(fact) => {
                        facts.push(fact.clone());
                        match explicit {
                            Some(alive) => Observation {
                                alive,
                                last_seen_alive_at: if alive {
                                    Some(now)
                                } else {
                                    previous_entry.and_then(|old| old.last_seen_alive_at)
                                },
                                session_observation: Some(
                                    if alive {
                                        SessionObservation::Loaded
                                    } else {
                                        SessionObservation::NotLoaded
                                    }
                                    .as_str()
                                    .to_string(),
                                ),
                                observed_at: Some(now.to_rfc3339()),
                            },
                            // 缺字段：**不**写展示态，也**不**借默认值刷新时间。
                            None => Observation {
                                alive: fact.alive,
                                last_seen_alive_at: previous_entry
                                    .and_then(|old| old.last_seen_alive_at),
                                session_observation: None,
                                observed_at: None,
                            },
                        }
                    }
                    // 这一拍完全没提到它。
                    None => Observation {
                        alive: false,
                        last_seen_alive_at: previous_entry.and_then(|old| old.last_seen_alive_at),
                        session_observation: None,
                        observed_at: None,
                    },
                }
            }
        };
        // 从来没有记录、这一拍也没提到：不插入。缺记录不是死亡的证据，
        // 也不能凭它编出一条 false 去影响租约默认规则。
        if previous_entry.is_none()
            && matches!(input.loaded, None)
            && !input.legacy.iter().any(|fact| &fact.thread_id == thread_id)
        {
            continue;
        }
        file.threads.insert(thread_id.clone(), entry);
    }

    NormalizedObservation { facts, file }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{LoadedThreadFacts, parse_observation_input};

    /// 契约文件：宿主与插件两侧测试都用 `include_str!` 读同一份。
    const DRIVER_CONTRACT: &str = include_str!("../contract/driver-protocol.json");

    /// 缺新字段 = 旧宿主，`loaded` 必须是 `None`；空完整数组 = 热表确实为空，
    /// 必须是 `Some([])`。这两者混淆一次，就会把「没能力回答」当成「一个都没加载」。
    #[test]
    fn kanban_observation_empty_complete_is_not_legacy() {
        let legacy = parse_observation_input(&json!({"threads": []})).unwrap();
        let complete =
            parse_observation_input(&json!({"threads": [], "loaded_threads": []})).unwrap();
        assert!(legacy.loaded.is_none());
        assert_eq!(complete.loaded, Some(Vec::new()));
    }

    /// 显式给出的 `loaded_threads` 只要形状不对就是无效快照，不能降级成空数组：
    /// null、对象、缺 `idle`、空白 ID、`idle` 不是 boolean、重复 ID。
    #[test]
    fn kanban_observation_invalid_complete_is_rejected() {
        for value in [
            json!(null),
            json!({}),
            json!([{"thread_id": "t"}]),
            json!([{"thread_id": " ", "idle": false}]),
            json!([{"thread_id": "t", "idle": "yes"}]),
            json!([
                {"thread_id": "t", "idle": false},
                {"thread_id": "t", "idle": true}
            ]),
        ] {
            assert!(
                parse_observation_input(&json!({"threads": [], "loaded_threads": value})).is_err(),
                "必须拒绝无效的 loaded_threads：{value}"
            );
        }
    }

    /// 缺 `alive` 的旧线程仍按「还活着」调度（与今天一致），但它**不是**宿主
    /// 对这一拍的报告：不得进 `reported_alive`，否则默认值会变成加载证据。
    #[test]
    fn kanban_observation_default_alive_is_not_reported_alive() {
        let input =
            parse_observation_input(&json!({"threads":[{"thread_id":"t","idle":false}]})).unwrap();
        assert!(input.legacy[0].alive);
        assert!(!input.reported_alive.contains_key("t"));
    }

    /// 旧 `threads[]` 的调度语义逐字保持：缺 `idle` 当不闲、缺 `alive` 当活着，
    /// 显式 boolean `alive` 才进 `reported_alive`（false 也要记）。
    #[test]
    fn kanban_observation_legacy_defaults_and_explicit_alive() {
        let input = parse_observation_input(&json!({"threads": [
            {"thread_id": "no-idle", "alive": false},
            {"thread_id": "no-alive", "idle": true},
            {"thread_id": "both", "idle": false, "alive": true},
        ]}))
        .unwrap();

        assert_eq!(input.legacy.len(), 3);
        assert_eq!(input.legacy[0].thread_id, "no-idle");
        assert!(!input.legacy[0].idle, "缺 idle 必须当不闲");
        assert!(!input.legacy[0].alive, "显式 false 必须原样保留");
        assert_eq!(input.legacy[1].thread_id, "no-alive");
        assert!(input.legacy[1].idle);
        assert!(input.legacy[1].alive, "缺 alive 必须当活着");
        assert_eq!(input.legacy[2].thread_id, "both");

        assert_eq!(input.reported_alive.get("no-idle"), Some(&false));
        assert_eq!(
            input.reported_alive.get("no-alive"),
            None,
            "缺 alive 不是宿主报告"
        );
        assert_eq!(input.reported_alive.get("both"), Some(&true));
    }

    /// 合法的完整快照逐项保留：ID 原样（不 trim、不改大小写）、允许未知附加键。
    #[test]
    fn kanban_observation_loaded_keeps_ids_verbatim_and_ignores_extra_keys() {
        let input = parse_observation_input(&json!({
            "threads": [],
            "loaded_threads": [
                {"thread_id": " thread-1 ", "idle": true, "title": "ignored"},
                {"thread_id": "thread-2", "idle": false},
                {"thread_id": "Thread-3", "idle": true},
            ],
        }))
        .unwrap();

        assert_eq!(
            input.loaded,
            Some(vec![
                LoadedThreadFacts {
                    thread_id: " thread-1 ".to_string(),
                    idle: true,
                },
                LoadedThreadFacts {
                    thread_id: "thread-2".to_string(),
                    idle: false,
                },
                LoadedThreadFacts {
                    thread_id: "Thread-3".to_string(),
                    idle: true,
                },
            ])
        );
    }

    /// 错误必须能定位到具体条目和原因，且只含契约字段名，不回显任何内容。
    #[test]
    fn kanban_observation_errors_name_the_index_and_reason() {
        let cases = [
            (json!(null), "must be an array"),
            (json!({}), "must be an array"),
            (
                json!([{"thread_id": "t"}]),
                "loaded_threads[0] needs boolean idle",
            ),
            (
                json!([{"thread_id": 7, "idle": false}]),
                "loaded_threads[0] needs a nonblank thread_id",
            ),
            (
                json!([{"thread_id": "\t\n", "idle": false}]),
                "loaded_threads[0] needs a nonblank thread_id",
            ),
            (
                json!([{"thread_id": "a", "idle": true}, {"thread_id": "a", "idle": false}]),
                "loaded_threads[1] duplicates thread_id",
            ),
        ];
        for (value, expected) in cases {
            let error = parse_observation_input(&json!({"threads": [], "loaded_threads": value}))
                .expect_err("无效快照必须报错");
            assert!(error.contains(expected), "{error} 应包含 {expected}");
            assert!(
                error.starts_with("driver/next loaded_threads"),
                "错误必须指明是 driver/next 的 loaded_threads：{error}"
            );
        }
    }

    /// 旧 `threads[]` 缺数组或条目缺 ID 仍是调用方的错（与今天一致）。
    #[test]
    fn kanban_observation_requires_the_legacy_threads_array() {
        let error =
            parse_observation_input(&json!({"results": []})).expect_err("缺 threads 必须报错");
        assert!(error.contains("threads"), "{error}");

        let error = parse_observation_input(&json!({"threads": [{"idle": true}]}))
            .expect_err("条目缺 thread_id 必须报错");
        assert!(error.contains("thread_id"), "{error}");

        let error = parse_observation_input(&json!({"threads": {}})).expect_err("非数组必须报错");
        assert!(error.contains("threads"), "{error}");
    }

    /// 契约里的请求样例必须能被本解析器读进来：`request` 带完整快照，
    /// `legacy_request` 是它删掉新字段后的旧载荷（必须解析为 `loaded=None`）。
    #[test]
    fn kanban_observation_contract_samples_parse_both_ways() {
        let contract: serde_json::Value =
            serde_json::from_str(DRIVER_CONTRACT).expect("契约必须是合法 JSON");

        let request = &contract["request"];
        let loaded = request["loaded_threads"]
            .as_array()
            .expect("request 必须带 loaded_threads 样例");
        assert!(!loaded.is_empty(), "样例不该是空数组：{loaded:?}");
        let complete = parse_observation_input(request).expect("request 样例必须解析成功");
        assert!(
            complete.loaded.is_some(),
            "request 样例必须声明完整快照：{complete:?}"
        );

        let legacy_request = &contract["legacy_request"];
        let legacy =
            parse_observation_input(legacy_request).expect("legacy_request 样例必须解析成功");
        assert!(
            legacy.loaded.is_none(),
            "legacy_request 必须是没有 loaded_threads 的旧载荷"
        );
        assert!(
            legacy_request.get("loaded_threads").is_none(),
            "legacy_request 不得带新字段：{legacy_request}"
        );
        assert_eq!(
            legacy_request["threads"], request["threads"],
            "legacy_request 的 threads 必须与 request 一致"
        );
        assert_eq!(
            legacy_request["results"], request["results"],
            "legacy_request 的 results 必须与 request 一致"
        );
        assert_eq!(
            legacy_request["sessions"], request["sessions"],
            "legacy_request 的 sessions 必须与 request 一致"
        );
    }

    use crate::observation::{ObservationFile, SessionObservation};

    use super::normalize;

    fn at(hour: u32, minute: u32) -> chrono::DateTime<chrono::Local> {
        chrono::DateTime::parse_from_rfc3339(&format!("2026-11-03T{hour:02}:{minute:02}:00+08:00"))
            .unwrap()
            .with_timezone(&chrono::Local)
    }

    fn relevant(ids: &[&str]) -> std::collections::BTreeSet<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    /// K3：完整快照胜过陈旧的 driver 子集（同 ID 子集说 false，快照说在）。
    #[test]
    fn kanban_observation_complete_overrides_stale_subset() {
        let input = parse_observation_input(&serde_json::json!({
            "threads":[{"thread_id":"t","alive":false,"idle":false}],
            "loaded_threads":[{"thread_id":"t","idle":true},{"thread_id":"other","idle":false}]
        }))
        .unwrap();
        let normalized = normalize(
            &input,
            &relevant(&["t"]),
            &ObservationFile::default(),
            at(10, 0),
        );
        assert_eq!(normalized.facts.len(), 1);
        assert!(normalized.facts[0].alive && normalized.facts[0].idle);
        assert_eq!(
            normalized.file.threads["t"].presentation(),
            SessionObservation::Loaded
        );
        assert!(
            !normalized.file.threads.contains_key("other"),
            "无关 ID 不得被持久化"
        );
    }

    /// 完整快照里没有的已知绑定 ID：明确 not_loaded，且不得沿用旧 idle 推断
    /// 「跑完了」——未加载不证明工具已经停止。
    #[test]
    fn kanban_observation_complete_absence_is_not_loaded_and_not_idle() {
        let input = parse_observation_input(&serde_json::json!({
            "threads":[{"thread_id":"t","alive":true,"idle":true}],
            "loaded_threads":[]
        }))
        .unwrap();
        let normalized = normalize(
            &input,
            &relevant(&["t"]),
            &ObservationFile::default(),
            at(10, 0),
        );
        assert_eq!(normalized.facts.len(), 1);
        assert!(!normalized.facts[0].alive, "the complete snapshot wins");
        assert!(!normalized.facts[0].idle, "absence is not idleness");
        let entry = &normalized.file.threads["t"];
        assert_eq!(entry.presentation(), SessionObservation::NotLoaded);
        assert_eq!(
            entry.observed_at.as_deref(),
            Some(at(10, 0).to_rfc3339().as_str())
        );
        assert_eq!(entry.last_seen_alive_at, None, "never seen loaded");
    }

    /// 旧宿主（无完整快照）**显式**报 `alive:false` 也是展示证据：它就是
    /// 「当前宿主没加载这个会话」，但历史可能仍在。调度沿用同一份 alive/idle。
    #[test]
    fn kanban_observation_legacy_explicit_false_is_not_loaded() {
        let input = parse_observation_input(&serde_json::json!({
            "threads":[{"thread_id":"t","alive":false,"idle":true}]
        }))
        .unwrap();
        let previous = ObservationFile::default();
        let normalized = normalize(&input, &relevant(&["t"]), &previous, at(10, 0));
        assert_eq!(normalized.facts.len(), 1);
        assert!(!normalized.facts[0].alive);
        assert!(normalized.facts[0].idle);
        let entry = &normalized.file.threads["t"];
        assert_eq!(entry.presentation(), SessionObservation::NotLoaded);
        assert_eq!(entry.presentation_at(), Some(at(10, 0)));
        assert_eq!(
            entry.last_seen_alive_at, None,
            "an explicit false must not invent a last-alive time"
        );
    }

    /// K6：旧宿主**没上报**这个 ID（这一拍压根没提）而文件里只有旧的
    /// `alive:false`：展示 unknown——旧 false 不是当前宿主的新死亡证明。
    #[test]
    fn kanban_observation_old_false_without_a_fresh_report_is_unknown() {
        let input = parse_observation_input(&serde_json::json!({"threads":[]})).unwrap();
        let mut previous = ObservationFile::default();
        // 模拟旧格式文件：只有 alive 与历史时间，没有新字段。
        previous.threads.insert(
            "t".to_string(),
            crate::observation::Observation {
                alive: false,
                last_seen_alive_at: Some(at(8, 35)),
                session_observation: None,
                observed_at: None,
            },
        );
        let normalized = normalize(&input, &relevant(&["t"]), &previous, at(10, 0));
        let entry = &normalized.file.threads["t"];
        assert_eq!(entry.presentation(), SessionObservation::Unknown);
        assert_eq!(entry.observed_at, None);
        assert_eq!(entry.last_seen_alive_at, Some(at(8, 35)));
    }

    /// K15：缺 `alive` 的旧条目按「还活着」调度，但不是报告：不写展示态、
    /// 不刷最后已加载时间。
    #[test]
    fn kanban_observation_missing_alive_does_not_fabricate_a_report() {
        let input = parse_observation_input(&serde_json::json!({
            "threads":[{"thread_id":"t","idle":false}]
        }))
        .unwrap();
        let mut previous = ObservationFile::default();
        previous.observe("t", true, at(8, 35));
        let normalized = normalize(&input, &relevant(&["t"]), &previous, at(10, 0));
        assert!(normalized.facts[0].alive, "scheduling default is unchanged");
        let entry = &normalized.file.threads["t"];
        assert_eq!(entry.presentation(), SessionObservation::Unknown);
        assert_eq!(entry.observed_at, None);
        assert_eq!(
            entry.last_seen_alive_at,
            Some(at(8, 35)),
            "the historical last-alive time must survive untouched"
        );
    }

    /// K12：显式 alive=true 每拍都会刷新「观测到」的时间，但只有 first sighting
    /// 才刷新「最后已加载」。同一会话连续两拍 idle 不应被读成「刚刚活动过」。
    #[test]
    fn kanban_observation_repeated_loaded_refreshes_only_the_observation_time() {
        let first = parse_observation_input(&serde_json::json!({
            "threads":[{"thread_id":"t","alive":true,"idle":true}]
        }))
        .unwrap();
        let one = normalize(
            &first,
            &relevant(&["t"]),
            &ObservationFile::default(),
            at(10, 0),
        );
        assert_eq!(
            one.file.threads["t"].presentation(),
            SessionObservation::Loaded
        );
        assert_eq!(one.file.threads["t"].last_seen_alive_at, Some(at(10, 0)));

        let second = parse_observation_input(&serde_json::json!({
            "threads":[{"thread_id":"t","alive":true,"idle":true}]
        }))
        .unwrap();
        let two = normalize(&second, &relevant(&["t"]), &one.file, at(11, 30));
        assert_eq!(
            two.file.threads["t"].presentation(),
            SessionObservation::Loaded
        );
        assert_eq!(
            two.file.threads["t"].presentation_at(),
            Some(at(11, 30)),
            "the observation time advances with every successful tick"
        );
        assert_eq!(
            two.file.threads["t"].last_seen_alive_at,
            Some(at(11, 30)),
            "a still-loaded session is still reported as last seen alive"
        );
    }

    /// 旧记录里存在、这一拍完全没提到的相关 ID：按旧的 write_observations 规则
    /// 记 alive=false，但展示降级 unknown，最后 alive 时间保留。
    #[test]
    fn kanban_observation_dropped_from_legacy_report_becomes_unknown() {
        let input = parse_observation_input(&serde_json::json!({"threads":[]})).unwrap();
        let mut previous = ObservationFile::default();
        previous.observe("t", true, at(8, 0));
        let normalized = normalize(&input, &relevant(&["t"]), &previous, at(10, 0));
        assert_eq!(
            normalized.file.threads["t"].presentation(),
            SessionObservation::Unknown
        );
        assert!(!normalized.file.threads["t"].alive);
        assert_eq!(
            normalized.file.threads["t"].last_seen_alive_at,
            Some(at(8, 0))
        );
        assert!(
            normalized.facts.is_empty(),
            "an unmentioned legacy id is not a scheduling fact"
        );
    }

    /// 此前完全没有记录的相关 ID：既不插 false，也不出现在调度事实里
    /// （保留既有「缺记录」的租约默认规则）。
    #[test]
    fn kanban_observation_never_seen_and_unmentioned_is_not_inserted() {
        let input = parse_observation_input(&serde_json::json!({"threads":[]})).unwrap();
        let normalized = normalize(
            &input,
            &relevant(&["t"]),
            &ObservationFile::default(),
            at(10, 0),
        );
        assert!(normalized.facts.is_empty());
        assert!(!normalized.file.threads.contains_key("t"));
    }

    /// 无关旧记录按相关集合剪掉；`previous` 是只读输入，不得被就地改写。
    #[test]
    fn kanban_observation_prunes_unrelated_and_never_mutates_previous() {
        let input = parse_observation_input(&serde_json::json!({
            "threads":[{"thread_id":"t","alive":true,"idle":false}]
        }))
        .unwrap();
        let mut previous = ObservationFile::default();
        previous.observe("t", true, at(8, 0));
        previous.observe("orphan", true, at(8, 0));
        let before = format!("{previous:?}");
        let normalized = normalize(&input, &relevant(&["t"]), &previous, at(10, 0));
        assert_eq!(
            format!("{previous:?}"),
            before,
            "previous must stay read-only"
        );
        assert!(!normalized.file.threads.contains_key("orphan"));
        assert!(normalized.file.threads.contains_key("t"));
    }

    /// 同一拍里 loaded 与 legacy 同时提到一个 ID 且二者冲突：完整快照胜出，
    /// 展示不得被旧子集推翻。
    #[test]
    fn kanban_observation_complete_beats_conflicting_legacy_alive() {
        let input = parse_observation_input(&serde_json::json!({
            "threads":[{"thread_id":"t","alive":true,"idle":true}],
            "loaded_threads":[]
        }))
        .unwrap();
        let normalized = normalize(
            &input,
            &relevant(&["t"]),
            &ObservationFile::default(),
            at(10, 0),
        );
        assert!(!normalized.facts[0].alive);
        assert_eq!(
            normalized.file.threads["t"].presentation(),
            SessionObservation::NotLoaded
        );
    }
}
