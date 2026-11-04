//! 旧看板数据接管：把一份旧状态目录搬进受管项目，**一个字节都不丢**。
//!
//! # 迁移的是数据，不是授权
//!
//! 旧安装可能带着一个"开关是开的"的偏好文件、一条常驻登记、PATH 上一个旧二进制。
//! 这些**都不算**新授权（设计 §11、迁移子 spec §8.3）：接管只搬卡片与项目配置，
//! 权限要由用户重新确认。所以这里不读、不写、也不推断任何授权状态。
//!
//! # 未知格式不猜
//!
//! 旧 `board.json` **没有版本号字段**（`Board` 就是裸的 `{cards, next_order}`）。
//! 这意味着"格式是不是我认识的"只能靠**严格结构校验**回答：
//!
//! - 必须是 JSON 对象，且**只有** `cards` 与 `next_order` 两个键；
//! - 每张卡必须带齐这个版本认识的必填字段，且状态名必须在枚举里；
//! - 任何一处不符 → 拒绝迁移并说明原因，**不**回落为空队列。
//!
//! 最后一条是关键。`persist::load_board` 在推进循环里故意宽容（坏文件当空队列，
//! 免得一次坏读停掉整个看板），但在迁移里用同样的宽容等于把用户的卡**静默删掉**：
//! 接管成功、看板空的、没有任何报错。两个调用点的正确行为相反，所以这条严格
//! 校验必须独立实现，不能复用 `load_board`。
//!
//! # 瞬态资源不进备份
//!
//! `plugin.lock` / `board.lock` / socket / 租约都是"某个进程此刻在做什么"的记录。
//! 搬过去只会得到幽灵持有者：锁文件指向早就不存在的进程，而存活判定只认 `flock`，
//! 于是它永远显示"仍被持有"，屏障永远等不完（设计 §9.2、G2 §8.1）。

use std::path::{Path, PathBuf};

use superpowers_kanban_core::board::Board;
use superpowers_kanban_core::card::{Card, CardId, CardKind, CardState};

/// 旧状态目录里**会**被搬走的文件。
///
/// 只列这些，不扫目录：一份来历不明的文件被顺手搬进受管项目，比留在原地更危险。
const MIGRATED_FILES: [&str; 1] = ["board.json"];

/// 旧状态目录里**明确不**搬的文件前缀/名字。
///
/// 列出来是为了让"为什么不搬"变成可读的代码而不是口头约定；`scan` 用它给每个
/// 被跳过的文件一个理由，报告里能逐条解释。
///
/// `preferences.json` 出现在这张表里不是"丢掉配置"，而是"配置本来就不在这里"：
/// 项目设置住在**状态目录之外**的 `<workdir>/.yi-agent/preferences.json`
/// （`layout::project_preferences_path`）。只往 `<target>` 写 `board.json`
/// 就自动保住了它，配置因此不需要"—复制"，也就不可能被复制坏。
const TRANSIENT: [&str; 6] = [
    "plugin.lock",
    "board.lock",
    "operations.lock",
    "preferences.json",
    "superpowers-kanban.sock",
    "inbox",
];

/// 一份待迁移的旧状态目录的体检结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyScan {
    /// 旧状态目录。
    pub state_dir: PathBuf,
    /// 卡片数量（`board.json` 可解析时）。
    pub cards: usize,
    /// 会被搬走的文件（相对路径）。
    pub migrated: Vec<String>,
    /// 被跳过的文件，附原因。
    pub skipped: Vec<Skipped>,
}

/// 一个被跳过的文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

/// 迁移失败的原因。每一种都可定位到具体文件或字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrateError {
    /// 旧状态目录不存在，或不是一个目录。
    NoSuchStateDir(PathBuf),
    /// 旧看板文件读不出来（权限、编码）。
    Unreadable { path: PathBuf, detail: String },
    /// 旧看板文件不是一个认识的格式。**绝不**当作空看板继续。
    Unrecognized { path: PathBuf, detail: String },
    /// 目标目录已存在且非空：接管到一个已有数据的项目会覆盖用户的东西。
    TargetNotEmpty(PathBuf),
    /// 写盘失败。
    Io { path: PathBuf, detail: String },
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrateError::NoSuchStateDir(path) => {
                write!(f, "legacy state dir does not exist: {}", path.display())
            }
            MigrateError::Unreadable { path, detail } => {
                write!(f, "cannot read {}: {detail}", path.display())
            }
            MigrateError::Unrecognized { path, detail } => write!(
                f,
                "{} is not a board format this version recognizes ({detail}); refusing to \
                 guess rather than adopting an empty board",
                path.display()
            ),
            MigrateError::TargetNotEmpty(path) => write!(
                f,
                "target already has data ({}); adopting over it would destroy it",
                path.display()
            ),
            MigrateError::Io { path, detail } => {
                write!(f, "cannot write {}: {detail}", path.display())
            }
        }
    }
}

impl std::error::Error for MigrateError {}

/// 体检一份旧状态目录：**只读**，不搬任何东西。
///
/// 这是接管的"看"这一步：先让用户看到会搬什么、会跳过什么，再决定要不要搬。
pub fn scan(state_dir: &Path) -> Result<LegacyScan, MigrateError> {
    if !state_dir.is_dir() {
        return Err(MigrateError::NoSuchStateDir(state_dir.to_path_buf()));
    }
    let board_path = state_dir.join("board.json");
    let cards = if board_path.is_file() {
        read_legacy_board(&board_path)?.cards_len()
    } else {
        0
    };

    let mut migrated = Vec::new();
    let mut skipped = Vec::new();
    let entries = std::fs::read_dir(state_dir).map_err(|error| MigrateError::Unreadable {
        path: state_dir.to_path_buf(),
        detail: error.to_string(),
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| MigrateError::Unreadable {
            path: state_dir.to_path_buf(),
            detail: error.to_string(),
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if MIGRATED_FILES.contains(&name.as_str()) {
            migrated.push(name);
        } else {
            let reason = if TRANSIENT.contains(&name.as_str()) {
                "transient: a lock, socket, inbox or preference records what some process \
                 was doing, not what the board knows"
                    .to_string()
            } else {
                "not a board data file: adopting unknown files into a managed project is \
                 riskier than leaving them where they are"
                    .to_string()
            };
            skipped.push(Skipped { path: name, reason });
        }
    }
    // 稳定顺序：报告与测试断言都不该依赖 `read_dir` 的返回次序。
    migrated.sort();
    skipped.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(LegacyScan {
        state_dir: state_dir.to_path_buf(),
        cards,
        migrated,
        skipped,
    })
}

/// 把旧状态目录里的看板搬进 `target_dir`，先备份。
///
/// 顺序是刻意的：
///
/// 1. 体检（`scan`）与严格解析——**任何不认识的格式都在这一步失败**，不写盘；
/// 2. 目标目录必须为空或不存在——接管到有数据的项目会覆盖用户的东西；
/// 3. 旧文件先备份到 `<target>.legacy-backup-<unix>`，再原子写新文件；
/// 4. 写失败时把备份恢复回去，不留下半接管状态。
///
/// 返回备份目录，便于调用方在失败排查时指引用户。
pub fn adopt(
    state_dir: &Path,
    target_dir: &Path,
    now: chrono::DateTime<chrono::Local>,
) -> Result<PathBuf, MigrateError> {
    let scan = scan(state_dir)?;
    if !scan.migrated.iter().any(|name| name == "board.json") {
        // 没有 board.json 就没什么可接管的。这不必然是错误（一个从未启动过的旧
        // 安装就是这样），但也不能假装搬成功。
        return Err(MigrateError::Unrecognized {
            path: state_dir.join("board.json"),
            detail: "the legacy state dir has no board.json; there are no cards to adopt"
                .to_string(),
        });
    }
    let board = read_legacy_board(&state_dir.join("board.json"))?;

    if target_dir.exists() {
        let empty = std::fs::read_dir(target_dir)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if !empty {
            return Err(MigrateError::TargetNotEmpty(target_dir.to_path_buf()));
        }
    }

    let backup = backup_dir(target_dir, now);
    std::fs::create_dir_all(&backup).map_err(|error| MigrateError::Io {
        path: backup.clone(),
        detail: error.to_string(),
    })?;
    // 备份先落地，再动目标：反向顺序会在"备份失败但目标已被改"时丢掉原数据。
    for name in &scan.migrated {
        let from = state_dir.join(name);
        let to = backup.join(name);
        std::fs::copy(&from, &to).map_err(|error| MigrateError::Io {
            path: to,
            detail: error.to_string(),
        })?;
    }

    let write_result = std::fs::create_dir_all(target_dir)
        .map_err(|error| MigrateError::Io {
            path: target_dir.to_path_buf(),
            detail: error.to_string(),
        })
        .and_then(|()| {
            // 用 core 的 `Board` 重新序列化，而不是原样拷贝旧字节：旧文件可能带着
            // 这个版本已不认识的键，原样搬过去会让下一个读者继续困惑。
            let bytes = serde_json::to_vec_pretty(&board).map_err(|error| MigrateError::Io {
                path: target_dir.join("board.json"),
                detail: error.to_string(),
            })?;
            write_atomic(&target_dir.join("board.json"), &bytes)
        });

    if let Err(error) = write_result {
        // 恢复：把刚写进去的东西删掉，让目标回到"空或不存在"。
        let _ = std::fs::remove_file(target_dir.join("board.json"));
        return Err(error);
    }
    Ok(backup)
}

/// 原子写：同目录临时文件 + rename，读者永远看不到半个文件。
///
/// 自己实现而不是复用 `persist::save_board`：那个函数把 `Board` 序列化成
/// 它自己的形状，而这里已经算好了字节（迁移要写的是**校验过**的那一份）。
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), MigrateError> {
    let temp = path.with_extension("json.adopting");
    let write = |temp: &Path| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::File::create(temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    };
    if let Err(error) = write(&temp) {
        let _ = std::fs::remove_file(&temp);
        return Err(MigrateError::Io {
            path: temp,
            detail: error.to_string(),
        });
    }
    std::fs::rename(&temp, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        MigrateError::Io {
            path: path.to_path_buf(),
            detail: error.to_string(),
        }
    })
}

fn backup_dir(target: &Path, now: chrono::DateTime<chrono::Local>) -> PathBuf {
    let stamp = now.format("%Y%m%dT%H%M%S");
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "board".to_string());
    target.with_file_name(format!("{name}.legacy-backup-{stamp}"))
}

/// 严格解析旧看板：只认 `{cards, next_order}`，其余一律拒绝。
///
/// 用 `serde_json::Value` 先看形状，再交给 `Board` 反序列化。两步是必要的：
/// `Board` 的字段都是私有的且没有 `deny_unknown_fields`，单靠反序列化会**接受**
/// 一个多了未知键的文件——那正是"格式其实变了但我没发现"的情形。
fn read_legacy_board(path: &Path) -> Result<Board, MigrateError> {
    let text = std::fs::read_to_string(path).map_err(|error| MigrateError::Unreadable {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| MigrateError::Unrecognized {
            path: path.to_path_buf(),
            detail: format!("not JSON: {error}"),
        })?;
    let object = value
        .as_object()
        .ok_or_else(|| MigrateError::Unrecognized {
            path: path.to_path_buf(),
            detail: "the top level is not an object".to_string(),
        })?;
    let unknown: Vec<&String> = object
        .keys()
        .filter(|key| !matches!(key.as_str(), "cards" | "next_order"))
        .collect();
    if !unknown.is_empty() {
        return Err(MigrateError::Unrecognized {
            path: path.to_path_buf(),
            detail: format!("unexpected top-level keys: {unknown:?}"),
        });
    }
    if !object.contains_key("cards") {
        return Err(MigrateError::Unrecognized {
            path: path.to_path_buf(),
            detail: "no `cards` array".to_string(),
        });
    }
    // 先逐张校验必填字段，把"哪张卡坏了"说清楚，再整体反序列化。
    if let Some(cards) = object["cards"].as_array() {
        for (index, card) in cards.iter().enumerate() {
            validate_card_shape(card).map_err(|detail| MigrateError::Unrecognized {
                path: path.to_path_buf(),
                detail: format!("card #{index}: {detail}"),
            })?;
        }
    }
    serde_json::from_value::<Board>(value).map_err(|error| MigrateError::Unrecognized {
        path: path.to_path_buf(),
        detail: format!("does not deserialize as a board: {error}"),
    })
}

/// 一张旧卡必须有这个版本认识的每一个必填字段。
fn validate_card_shape(card: &serde_json::Value) -> Result<(), String> {
    let card = card.as_object().ok_or("not an object")?;
    for key in [
        "id",
        "spec_path",
        "plan_path",
        "state",
        "enqueued_at",
        "order",
    ] {
        if !card.contains_key(key) {
            return Err(format!("missing `{key}`"));
        }
    }
    let id = card["id"]
        .as_object()
        .and_then(|id| id.get("0"))
        .or_else(|| card["id"].as_str().map(|_| &card["id"]))
        .ok_or("`id` is neither a string nor a newtype")?;
    let id = id.as_str().ok_or("`id` is not a string")?;
    if id.is_empty() || id.contains('/') || id == ".." || id == "." {
        return Err(format!("`id` {id:?} is not a usable card id"));
    }
    let state = card["state"].as_str().ok_or("`state` is not a string")?;
    parse_state(state).ok_or_else(|| format!("`state` {state:?} is not a known state"))?;
    for key in ["spec_path", "plan_path"] {
        if !card[key].is_string() {
            return Err(format!("`{key}` is not a string"));
        }
    }
    if !card["order"].is_i64() {
        return Err("`order` is not an integer".to_string());
    }
    Ok(())
}

/// 旧状态名 → 当前枚举。只认 `CardState` 的 serde 拼法（snake_case）。
fn parse_state(raw: &str) -> Option<CardState> {
    Some(match raw {
        // 兜底：未知名字返回 `None` 由调用方报错，而不是落到某个默认状态。
        // 把不认识的 `paused` 当成 `queued` 会让迁移"成功"却改掉卡的含义。
        "pre_queued" => CardState::PreQueued,
        "queued" => CardState::Queued,
        "running" => CardState::Running,
        "needs_you" => CardState::NeedsYou,
        "awaiting_merge" => CardState::AwaitingMerge,
        "merge_queued" => CardState::MergeQueued,
        "failed" => CardState::Failed,
        "done" => CardState::Done,
        "cancelled" => CardState::Cancelled,
        _ => return None,
    })
}

trait CardsLen {
    fn cards_len(&self) -> usize;
}

impl CardsLen for Board {
    fn cards_len(&self) -> usize {
        self.cards().len()
    }
}

/// 接管后的自检：每一张旧卡都在，且 id / 状态 / 次序逐张相同。
///
/// 用独立的比较函数而不是"迁移后重读一遍再比 `Board` 相等"：`Board` 内部还带着
/// `next_order`，一个为了保持次序而调整了它的实现会让比较失败，但那并不丢数据。
pub fn verify(old: &Board, new: &Board) -> Result<(), String> {
    let old_cards: Vec<&Card> = old.cards().iter().collect();
    let new_cards: Vec<&Card> = new.cards().iter().collect();
    if old_cards.len() != new_cards.len() {
        return Err(format!(
            "card count changed: {} -> {}",
            old_cards.len(),
            new_cards.len()
        ));
    }
    for (before, after) in old_cards.iter().zip(new_cards.iter()) {
        if before.id.0 != after.id.0 {
            return Err(format!("card order changed: {} -> {}", before.id, after.id));
        }
        if before.state != after.state {
            return Err(format!(
                "card {} changed state: {:?} -> {:?}",
                before.id, before.state, after.state
            ));
        }
        if before.order != after.order {
            return Err(format!(
                "card {} changed order: {} -> {}",
                before.id, before.order, after.order
            ));
        }
        if before.thread_id != after.thread_id {
            return Err(format!(
                "card {} lost its session link: {:?} -> {:?}",
                before.id, before.thread_id, after.thread_id
            ));
        }
        if before.spec_path != after.spec_path || before.plan_path != after.plan_path {
            return Err(format!("card {} changed its document paths", before.id));
        }
        if before.kind != after.kind {
            return Err(format!("card {} changed kind", before.id));
        }
    }
    Ok(())
}

/// 读一张卡当前应有的 id（给测试与诊断用，不参与迁移决策）。
pub fn card_ids(board: &Board) -> Vec<CardId> {
    board.cards().iter().map(|card| card.id.clone()).collect()
}

/// 一张卡是不是合并卡（迁移报告里区分两类卡）。
pub fn is_merge_card(card: &Card) -> bool {
    card.kind == CardKind::Merge
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at() -> chrono::DateTime<chrono::Local> {
        chrono::Local
            .with_ymd_and_hms(2026, 11, 2, 12, 0, 0)
            .single()
            .unwrap()
    }

    /// 造一份"旧安装"的状态目录：卡片 + 锁 + socket + 偏好 + inbox。
    fn legacy_state(dir: &Path) -> PathBuf {
        let state = dir.join("legacy/.yi-agent/superpowers-kanban");
        std::fs::create_dir_all(state.join("inbox")).unwrap();
        let mut board = Board::new();
        board.enqueue(
            CardId::new("impl-1"),
            dir.join("a.spec.md"),
            dir.join("a.plan.md"),
            at(),
        );
        board.enqueue(
            CardId::new("impl-2"),
            dir.join("b.spec.md"),
            dir.join("b.plan.md"),
            at(),
        );
        board
            .transition(&CardId::new("impl-1"), CardState::AwaitingMerge)
            .unwrap();
        board
            .set_thread_id(&CardId::new("impl-1"), "thread-9".to_string())
            .unwrap();
        std::fs::write(
            state.join("board.json"),
            serde_json::to_vec_pretty(&board).unwrap(),
        )
        .unwrap();
        for name in [
            "plugin.lock",
            "board.lock",
            "superpowers-kanban.sock",
            "preferences.json",
        ] {
            std::fs::write(state.join(name), "x").unwrap();
        }
        std::fs::write(state.join("inbox/delivered.json"), "{}").unwrap();
        state
    }

    #[test]
    fn scanning_a_legacy_dir_lists_what_moves_and_what_stays() {
        let dir = tempfile::tempdir().unwrap();
        let state = legacy_state(dir.path());

        let scan = scan(&state).unwrap();
        assert_eq!(scan.cards, 2);
        assert_eq!(scan.migrated, vec!["board.json".to_string()]);
        let skipped: Vec<&str> = scan.skipped.iter().map(|s| s.path.as_str()).collect();
        for name in [
            "plugin.lock",
            "board.lock",
            "superpowers-kanban.sock",
            "preferences.json",
            "inbox",
        ] {
            assert!(skipped.contains(&name), "{name} missing from {skipped:?}");
        }
        // 瞬态文件必须给出"为什么"而不是空字符串。
        for skipped in &scan.skipped {
            assert!(!skipped.reason.is_empty(), "{} has no reason", skipped.path);
        }
    }

    #[test]
    fn scanning_reads_only_and_moves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let state = legacy_state(dir.path());
        let before = std::fs::read(state.join("board.json")).unwrap();

        scan(&state).unwrap();

        assert_eq!(std::fs::read(state.join("board.json")).unwrap(), before);
    }

    #[test]
    fn adopting_preserves_ids_states_order_and_session_links() {
        let dir = tempfile::tempdir().unwrap();
        let state = legacy_state(dir.path());
        let target = dir.path().join("proj/.yi-agent/superpowers-kanban");
        let before = read_legacy_board(&state.join("board.json")).unwrap();

        adopt(&state, &target, at()).unwrap();

        let after = read_legacy_board(&target.join("board.json")).unwrap();
        verify(&before, &after).unwrap();
        assert_eq!(
            card_ids(&after),
            vec![CardId::new("impl-1"), CardId::new("impl-2")]
        );
        let first = after.get(&CardId::new("impl-1")).unwrap();
        assert_eq!(first.state, CardState::AwaitingMerge);
        assert_eq!(first.thread_id.as_deref(), Some("thread-9"));
    }

    #[test]
    fn adopting_backs_up_the_old_board_first() {
        let dir = tempfile::tempdir().unwrap();
        let state = legacy_state(dir.path());
        let target = dir.path().join("proj/.yi-agent/superpowers-kanban");

        let backup = adopt(&state, &target, at()).unwrap();

        assert!(backup.join("board.json").is_file(), "{backup:?}");
        let original = std::fs::read(state.join("board.json")).unwrap();
        assert_eq!(std::fs::read(backup.join("board.json")).unwrap(), original);
    }

    /// 迁移只写 `board.json`，**不**碰同级的 `preferences.json`。
    ///
    /// 这一条守的是 T23 Step 2 的"配置保持"：项目设置住在状态目录之外，所以
    /// 正确做法不是"记得把它一起搬"，而是"根本不写那个位置"。一旦有人让
    /// `adopt` 整个目录复制，这条会立刻红。
    #[test]
    fn adopting_leaves_the_sibling_project_preferences_alone() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        let state = project.join(".yi-agent").join("superpowers-kanban");
        let target = dir.path().join("managed");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("board.json"), r#"{"cards":[],"next_order":0}"#).unwrap();
        let preferences = project.join(".yi-agent").join("preferences.json");
        let settings = r#"{"superpowers_kanban":false,"concurrency":{"max":2}}"#;
        std::fs::write(&preferences, settings).unwrap();

        adopt(&state, &target, at()).unwrap();

        // 项目设置在原地，一个字节没变；也没有被"顺手复制"到新目录。
        assert_eq!(std::fs::read_to_string(&preferences).unwrap(), settings);
        assert!(!target.join("preferences.json").exists());
    }

    #[test]
    fn adopting_never_copies_a_lock_or_socket() {
        let dir = tempfile::tempdir().unwrap();
        let state = legacy_state(dir.path());
        let target = dir.path().join("proj/.yi-agent/superpowers-kanban");

        let backup = adopt(&state, &target, at()).unwrap();

        for name in ["plugin.lock", "board.lock", "superpowers-kanban.sock"] {
            assert!(
                !target.join(name).exists(),
                "{name} must not appear in the managed state dir"
            );
            assert!(
                !backup.join(name).exists(),
                "{name} must not be backed up either"
            );
        }
    }

    /// 一个**没有版本号**的旧文件：多出一个未知键就是"格式变了"。
    #[test]
    fn an_unknown_top_level_key_is_refused_instead_of_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("legacy");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(
            state.join("board.json"),
            r#"{"cards":[],"next_order":0,"schemaVersion":2}"#,
        )
        .unwrap();

        let error = adopt(&state, &dir.path().join("proj"), at()).unwrap_err();
        assert!(
            matches!(error, MigrateError::Unrecognized { .. }),
            "{error}"
        );
        assert!(
            error.to_string().contains("schemaVersion"),
            "the error must name the key it did not expect: {error}"
        );
    }

    #[test]
    fn an_unknown_card_state_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("legacy");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(
            state.join("board.json"),
            r#"{"cards":[{"id":"c1","spec_path":"a","plan_path":"b","state":"paused",
                "enqueued_at":"2026-11-02T12:00:00+08:00","order":1}],"next_order":2}"#,
        )
        .unwrap();

        let error = adopt(&state, &dir.path().join("proj"), at()).unwrap_err();
        assert!(error.to_string().contains("paused"), "{error}");
        assert!(
            !dir.path().join("proj/board.json").exists(),
            "nothing written"
        );
    }

    #[test]
    fn a_missing_required_field_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("legacy");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(
            state.join("board.json"),
            r#"{"cards":[{"id":"c1","spec_path":"a","state":"queued",
                "enqueued_at":"2026-11-02T12:00:00+08:00","order":1}],"next_order":2}"#,
        )
        .unwrap();

        let error = adopt(&state, &dir.path().join("proj"), at()).unwrap_err();
        assert!(error.to_string().contains("plan_path"), "{error}");
    }

    /// 坏文件绝不能被当成空看板接管——那等于静默删掉用户的卡。
    #[test]
    fn a_corrupt_board_is_refused_rather_than_treated_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("legacy");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("board.json"), "{not json").unwrap();

        let error = adopt(&state, &dir.path().join("proj"), at()).unwrap_err();
        assert!(
            matches!(error, MigrateError::Unrecognized { .. }),
            "{error}"
        );
        assert!(!dir.path().join("proj/board.json").exists());
        // 原文件必须保持原样，用户还能自己去看、去修。
        assert_eq!(
            std::fs::read_to_string(state.join("board.json")).unwrap(),
            "{not json"
        );
    }

    #[test]
    fn adopting_over_a_project_that_already_has_data_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = legacy_state(dir.path());
        let target = dir.path().join("proj/.yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("board.json"), r#"{"cards":[],"next_order":0}"#).unwrap();

        let error = adopt(&state, &target, at()).unwrap_err();
        assert!(matches!(error, MigrateError::TargetNotEmpty(_)), "{error}");
        // 目标里的东西一个字节都没被改。
        assert_eq!(
            std::fs::read_to_string(target.join("board.json")).unwrap(),
            r#"{"cards":[],"next_order":0}"#
        );
    }

    #[test]
    fn a_legacy_dir_without_a_board_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("legacy");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("plugin.lock"), "x").unwrap();

        let error = adopt(&state, &dir.path().join("proj"), at()).unwrap_err();
        assert!(error.to_string().contains("no board.json"), "{error}");
    }

    #[test]
    fn a_missing_legacy_dir_is_refused_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let error = scan(&dir.path().join("nope")).unwrap_err();
        assert!(error.to_string().contains("nope"), "{error}");
    }

    #[test]
    fn adopting_an_empty_target_dir_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let state = legacy_state(dir.path());
        let target = dir.path().join("proj/.yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&target).unwrap();

        adopt(&state, &target, at()).unwrap();
        assert!(target.join("board.json").is_file());
    }
}
