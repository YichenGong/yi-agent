//! 排空屏障的**只读**一半：插件如何得知"宿主正在停我"。
//!
//! # 为什么要看宿主的日志
//!
//! 排空的权威在宿主：它先 `begin_drain`，等到屏障前的回合全部收尾，才继续
//! 备份、迁移、提交。插件不是权威，但它必须知道这件事，因为**排空期间仍然
//! 要能收尾**：一张卡在排空开始时正跑到一半，那一轮还得能
//! `state --self awaiting_merge`、还得能 `merge-finish` 把名额还回去。把整个
//! CLI 一起封死会让那一轮永远收不完——它既不能写状态，也不能释放 permit，
//! 屏障于是永远等不到零。这是设计 §7 点名要避免的死锁。
//!
//! # 只读，不推断
//!
//! 宿主把进行中的事务记在 `<instance_home>/plugins/lifecycle/<plugin>.json`。
//! 本模块只做一件事：把这个文件的 `phase` 读出来，翻译成"排空中 / 已排空 /
//! 与我没关系"。它**不写**这个文件，也**不**靠进程名、mtime 或"最近没活动"
//! 去猜——猜错的方向是灾难性的：判成"正在排空"会让所有新卡都起不来，判成
//! "没排空"会让排空期间的写入穿过屏障。
//!
//! 三种结论的边界很清楚：
//!
//! | 日志 | 结论 |
//! |---|---|
//! | 不存在 | [`Drain::Idle`]：没有进行中的事务，正常推进 |
//! | 存在、属于本插件、`phase=done` | [`Drain::Idle`]：事务已收尾但文件还没删 |
//! | 存在、属于本插件、其它 phase | [`Drain::Draining`]：屏障仍在 |
//! | 存在、属于别的插件 | [`Drain::Idle`]：别的插件的事务与我无关 |
//! | 存在但读不懂 | [`Drain::Unknown`]：拒绝新自动任务，但允许收尾 |
//!
//! 最后一条是刻意的：一份读不懂的日志意味着"我不知道现在是不是在排空"。
//! 此时放新卡出去可能撞上正在备份的版本，而封死收尾则一定会造成上面那个
//! 死锁。所以只挡**新工作**，放行**收尾**。

use std::path::{Path, PathBuf};

/// 本模块认识的插件 id。与 `plugin.json` 的 `plugin` 字段一致。
pub const PLUGIN_ID: &str = "superpowers-kanban";

/// 一次排空判定的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drain {
    /// 没有进行中的排空：正常推进。
    Idle,
    /// 宿主正在停本插件：拒绝**新**自动任务，放行收尾。
    Draining {
        /// 宿主日志里的阶段名，原样带出（用于诊断，不解释）。
        phase: String,
    },
    /// 日志在、但读不懂：同样拒绝新自动任务，但说明的是"不知道"，不是"正在排"。
    Unknown {
        /// 给人看的原因。
        detail: String,
    },
    /// 日志在、但属于别的插件：与本插件无关。
    OtherPlugin,
}

impl Drain {
    /// 是否可以启动**新**的自动任务（新会话、新卡）。
    ///
    /// `Unknown` 也返回 `false`：不知道是否在排空时放行新工作，等于把"可能
    /// 撞上正在迁移的版本"当成"一定不会撞"。
    pub fn allows_new_work(&self) -> bool {
        matches!(self, Drain::Idle | Drain::OtherPlugin)
    }

    /// 是否仍然允许**收尾**类的写入（状态、合并名额释放）。
    ///
    /// 除"完全空闲"外的每一种结论都返回 `true`——包括读不懂的日志。理由见
    /// 模块文档：封死收尾会把排空自己锁死。
    pub fn allows_wrap_up(&self) -> bool {
        true
    }

    /// 给页面/诊断的一句话。`None` 表示无需解释。
    pub fn reason(&self) -> Option<String> {
        match self {
            Drain::Idle | Drain::OtherPlugin => None,
            Drain::Draining { phase } => Some(format!(
                "the host is draining this plugin (phase `{phase}`); new cards will not be \
                 launched until it finishes, but cards already running can still report their \
                 state"
            )),
            Drain::Unknown { detail } => Some(format!(
                "could not tell whether the host is draining this plugin ({detail}); new cards \
                 will not be launched, but cards already running can still report their state"
            )),
        }
    }
}

/// 宿主 lifecycle 日志的落点：`<instance_home>/plugins/lifecycle/<plugin>.json`。
///
/// 与 `yi-agent-plugins::JournalStore::path` 同一个拼法。插件不链接那个 crate
/// （对宿主零依赖），所以路径在这里重复一次；`drain.rs` 的测试用
/// `include_str!` 读宿主契约里的阶段清单，两边漂移会红。
pub fn journal_path(instance_home: &Path) -> PathBuf {
    instance_home
        .join("plugins")
        .join("lifecycle")
        .join(format!("{PLUGIN_ID}.json"))
}

/// 读宿主的事务日志，回答"现在是不是在停我"。
///
/// 纯读：`std::fs::read` 一次，不建目录、不加锁、不改文件。
pub fn read(instance_home: &Path) -> Drain {
    let path = journal_path(instance_home);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Drain::Idle,
        Err(error) => {
            return Drain::Unknown {
                detail: format!("{}: {error}", path.display()),
            };
        }
    };

    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return Drain::Unknown {
                detail: format!("{} is not readable JSON: {error}", path.display()),
            };
        }
    };

    classify(&value)
}

/// 把一份已经解析出来的日志翻译成结论。拆出来是为了让测试不必造文件。
pub fn classify(journal: &serde_json::Value) -> Drain {
    let plugin = journal.get("plugin").and_then(serde_json::Value::as_str);
    if plugin != Some(PLUGIN_ID) {
        // 没有 plugin 字段也算"不是我的"：一份说不出自己是谁的日志，不足以
        // 让我停掉自己的推进。
        return Drain::OtherPlugin;
    }
    let Some(phase) = journal.get("phase").and_then(serde_json::Value::as_str) else {
        return Drain::Unknown {
            detail: "the lifecycle journal has no `phase`".to_string(),
        };
    };
    // `done` 是事务的最后一个阶段：此时文件通常已经删掉，但崩溃恢复窗口里
    // 可能还在。把它当成"正在排空"会让插件在一次已完成的事务后永久停摆。
    if phase == "done" {
        return Drain::Idle;
    }
    if !KNOWN_PHASES.contains(&phase) {
        // 未知阶段（将来新增的）：不能当成"没事"，那会让屏障被绕过。
        return Drain::Unknown {
            detail: format!("unknown lifecycle phase `{phase}`"),
        };
    }
    Drain::Draining {
        phase: phase.to_string(),
    }
}

/// `lifecycle-v1.schema.json` 冻结的阶段清单。
///
/// 逐个列出而不是"除 done 之外都算排空"：后者会让一个拼错的阶段名（例如
/// `drain`）被当成合法排空阶段，永远等不到结束。与契约不一致时
/// `the_phase_list_matches_the_host_schema` 会红。
const KNOWN_PHASES: [&str; 8] = [
    "draining",
    "drained",
    "backing_up",
    "migrating",
    "health_checking",
    "committing",
    "projecting",
    "done",
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_journal(home: &Path, value: serde_json::Value) {
        let path = journal_path(home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    #[test]
    fn no_journal_means_no_drain() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), Drain::Idle);
        assert!(Drain::Idle.allows_new_work());
    }

    #[test]
    fn a_draining_journal_blocks_new_work_but_not_wrap_up() {
        let dir = tempfile::tempdir().unwrap();
        write_journal(
            dir.path(),
            json!({"plugin": PLUGIN_ID, "phase": "draining"}),
        );

        let drain = read(dir.path());
        assert_eq!(
            drain,
            Drain::Draining {
                phase: "draining".to_string()
            }
        );
        assert!(!drain.allows_new_work(), "no new card while draining");
        // 关键：收尾必须放行，否则那一轮永远写不完状态、放不掉 permit。
        assert!(
            drain.allows_wrap_up(),
            "a draining plugin must still let a running card report its state"
        );
        assert!(drain.reason().unwrap().contains("draining"));
    }

    #[test]
    fn every_intermediate_phase_blocks_new_work() {
        // 备份、迁移、健康检查都可能让旧版本不可用，一律不接新卡。
        let dir = tempfile::tempdir().unwrap();
        for phase in [
            "drained",
            "backing_up",
            "migrating",
            "health_checking",
            "committing",
            "projecting",
        ] {
            write_journal(dir.path(), json!({"plugin": PLUGIN_ID, "phase": phase}));
            let drain = read(dir.path());
            assert!(
                !drain.allows_new_work(),
                "phase `{phase}` must block new work"
            );
            assert!(drain.allows_wrap_up(), "phase `{phase}` must allow wrap-up");
        }
    }

    #[test]
    fn a_finished_transaction_is_not_a_drain() {
        // `done` 之后文件通常已删；崩溃恢复窗口里可能还在。当成排空会让插件
        // 在一次已完成的事务后永久停摆。
        let dir = tempfile::tempdir().unwrap();
        write_journal(dir.path(), json!({"plugin": PLUGIN_ID, "phase": "done"}));
        assert_eq!(read(dir.path()), Drain::Idle);
    }

    #[test]
    fn another_plugins_transaction_does_not_stop_this_one() {
        let dir = tempfile::tempdir().unwrap();
        write_journal(
            dir.path(),
            json!({"plugin": "something-else", "phase": "migrating"}),
        );
        assert_eq!(read(dir.path()), Drain::OtherPlugin);
        assert!(read(dir.path()).allows_new_work());
    }

    #[test]
    fn an_unreadable_journal_blocks_new_work_but_still_allows_wrap_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();

        let drain = read(dir.path());
        assert!(matches!(drain, Drain::Unknown { .. }), "{drain:?}");
        assert!(
            !drain.allows_new_work(),
            "not knowing must not mean 'go ahead'"
        );
        assert!(
            drain.allows_wrap_up(),
            "an unreadable journal must not deadlock a running card"
        );
    }

    #[test]
    fn an_unknown_phase_is_not_treated_as_idle() {
        // 将来新增的阶段：当成"没事"会让屏障被绕过。
        let dir = tempfile::tempdir().unwrap();
        write_journal(
            dir.path(),
            json!({"plugin": PLUGIN_ID, "phase": "teleporting"}),
        );
        assert!(matches!(read(dir.path()), Drain::Unknown { .. }));
    }

    #[test]
    fn a_misspelled_phase_is_rejected_rather_than_waited_on() {
        let dir = tempfile::tempdir().unwrap();
        write_journal(dir.path(), json!({"plugin": PLUGIN_ID, "phase": "drain"}));
        let drain = read(dir.path());
        assert!(
            matches!(&drain, Drain::Unknown { detail } if detail.contains("drain")),
            "{drain:?}"
        );
    }

    #[test]
    fn the_phase_list_matches_the_host_schema() {
        // 与宿主 `plugins/contracts/lifecycle-v1.schema.json` 的枚举逐项一致。
        // include_str! 让它成为编译期依赖：契约改名而这里没跟，测试就红。
        let schema = include_str!("../../../../contracts/lifecycle-v1.schema.json");
        let value: serde_json::Value = serde_json::from_str(schema).unwrap();
        let phases: Vec<&str> = value["properties"]["phase"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|phase| phase.as_str().unwrap())
            .collect();
        assert_eq!(phases, KNOWN_PHASES.to_vec());
    }

    #[test]
    fn reading_never_writes_anything() {
        // 只读是硬约束：宿主的事务日志由宿主独占。
        let dir = tempfile::tempdir().unwrap();
        let before: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        let _ = read(dir.path());
        let after: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(before.len(), after.len());
        assert!(!journal_path(dir.path()).exists());
    }
}
