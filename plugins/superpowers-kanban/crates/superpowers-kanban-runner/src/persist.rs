use std::io::Write;
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
}
