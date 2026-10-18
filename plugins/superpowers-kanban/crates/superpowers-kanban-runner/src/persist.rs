use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use superpowers_kanban_core::board::Board;

#[derive(Debug)]
pub enum PersistError {
    Io(std::io::Error),
    Encode(serde_json::Error),
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Io(error) => write!(f, "board state i/o error: {error}"),
            PersistError::Encode(error) => write!(f, "board state encode error: {error}"),
        }
    }
}

impl std::error::Error for PersistError {}

/// 读队列状态。文件缺失或损坏一律回退为**空队列**并继续——绝不 panic，
/// 也绝不因为一次坏读写就停掉推进循环。
pub fn load_board(path: &Path) -> Board {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 原子写：先写同目录的临时文件再 rename，读者永远看不到半个文件。
pub fn save_board(path: &Path, board: &Board) -> Result<(), PersistError> {
    let json = serde_json::to_vec_pretty(board).map_err(PersistError::Encode)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(PersistError::Io)?;
    }
    let temp = path.with_extension("json.tmp");
    {
        let mut file = std::fs::File::create(&temp).map_err(PersistError::Io)?;
        file.write_all(&json).map_err(PersistError::Io)?;
        file.sync_all().map_err(PersistError::Io)?;
    }
    std::fs::rename(&temp, path).map_err(PersistError::Io)
}

/// 看板数据的互斥门。语义与 `plugin.lock` **不同**：那把锁表达「谁是推进者」
/// （只有 `run` 取），这把锁表达「谁在读改写 board.json」（任何写者都取）。
/// 混用会让一次性的 CLI 动词被误判为「第二实例」而遭拒。
struct BoardLock {
    /// 关闭这个描述符就是全部释放协议，所以它从不被读——只为把锁按住而保活。
    _file: File,
}

/// 取 `<state_dir>/board.lock` 的独占锁，**阻塞**直到拿到。
///
/// 阻塞而非非阻塞：临界区只有「读约 15KB → 改 → 原子写」，毫秒级；与之相比
/// 让调用方处理 `WouldBlock` 重试反而更复杂。`flock` 由内核在持有进程退出
/// （含 `SIGKILL`）时释放，所以不会永久卡死。
///
/// 与 `merge_lock.rs` / `single_instance.rs` 同理由，**不在 drop 时 unlink**：
/// 删除文件会让另一个进程在旧 inode 上持锁、在新 inode 上再取到锁，等于没锁。
fn acquire_board_lock(state_dir: &Path) -> std::io::Result<BoardLock> {
    std::fs::create_dir_all(state_dir)?;
    let path = state_dir.join("board.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if outcome != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(BoardLock { _file: file })
}

/// 锁内 `load → 改 → save`。**任何**对 board.json 的读-改-写都必须经此。
///
/// 闭包返回 `Err` 时不落盘（失败不留半写状态）。这是 CLI 与 daemon 共用的
/// 唯一数据入口——正因如此，一次性 CLI 写入不会再被 daemon 的陈旧内存覆盖。
pub fn with_board<T>(
    state_dir: &Path,
    f: impl FnOnce(&mut Board) -> Result<T, String>,
) -> Result<T, String> {
    let _lock = acquire_board_lock(state_dir)
        .map_err(|error| format!("could not lock the board: {error}"))?;
    let path = state_dir.join("board.json");
    let mut board = load_board(&path);
    let value = f(&mut board)?;
    save_board(&path, &board).map_err(|error| error.to_string())?;
    Ok(value)
}

/// 只读一次看板。**不加锁**：`save_board` 用「同目录临时文件 + rename」落盘，
/// 读者永远看到完整文件（要么旧要么新，不会是半个）。`list` 走这里。
pub fn read_board(state_dir: &Path) -> Board {
    load_board(&state_dir.join("board.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};
    use superpowers_kanban_core::card::CardId;

    fn seeded() -> Board {
        let mut board = Board::new();
        board.enqueue(
            CardId::new("a"),
            "a.spec.md".into(),
            "a.plan.md".into(),
            Local
                .with_ymd_and_hms(2026, 10, 1, 9, 0, 0)
                .single()
                .unwrap(),
        );
        board
    }

    #[test]
    fn a_missing_file_yields_an_empty_board_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_board(&dir.path().join("nope.json")).is_empty());
    }

    #[test]
    fn a_corrupt_file_yields_an_empty_board_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_board(&path).is_empty());
    }

    #[test]
    fn a_saved_board_reads_back_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.json");
        save_board(&path, &seeded()).unwrap();
        let restored = load_board(&path);
        assert_eq!(restored.len(), 1);
        assert_eq!(
            restored.get(&CardId::new("a")).unwrap().plan_path,
            std::path::PathBuf::from("a.plan.md")
        );
    }

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.json");
        save_board(&path, &seeded()).unwrap();
        assert!(
            !path.with_extension("json.tmp").exists(),
            "temp file was renamed away"
        );
    }

    #[test]
    fn with_board_waits_for_the_lock_and_persists_the_change() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        // 先落一张空板。
        let mut board = Board::new();
        board.enqueue(
            CardId::new("a"),
            "a.spec.md".into(),
            "a.plan.md".into(),
            Local
                .with_ymd_and_hms(2026, 10, 1, 9, 0, 0)
                .single()
                .unwrap(),
        );
        save_board(&state_dir.join("board.json"), &board).unwrap();

        // 事务里改一处，落盘后必须读到。
        with_board(&state_dir, |board| {
            board
                .transition(
                    &CardId::new("a"),
                    superpowers_kanban_core::card::CardState::Running,
                )
                .map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        let reloaded = read_board(&state_dir);
        assert_eq!(
            reloaded.get(&CardId::new("a")).unwrap().state,
            superpowers_kanban_core::card::CardState::Running,
            "with_board 必须落盘"
        );
    }

    #[test]
    fn with_board_does_not_persist_when_the_closure_fails() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let mut board = Board::new();
        board.enqueue(
            CardId::new("a"),
            "a.spec.md".into(),
            "a.plan.md".into(),
            Local
                .with_ymd_and_hms(2026, 10, 1, 9, 0, 0)
                .single()
                .unwrap(),
        );
        save_board(&state_dir.join("board.json"), &board).unwrap();
        let before = std::fs::read_to_string(state_dir.join("board.json")).unwrap();

        let error = with_board(&state_dir, |board| {
            // 改了内存里的板，但闭包报错——一个字节都不该落盘。
            let _ = board.transition(
                &CardId::new("a"),
                superpowers_kanban_core::card::CardState::Cancelled,
            );
            Err::<(), String>("nope".to_string())
        })
        .unwrap_err();
        assert_eq!(error, "nope");
        let after = std::fs::read_to_string(state_dir.join("board.json")).unwrap();
        assert_eq!(before, after, "失败的闭包不得留下半写状态");
    }
}
