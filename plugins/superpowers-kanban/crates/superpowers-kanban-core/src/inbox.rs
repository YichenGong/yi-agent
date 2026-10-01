//! 把「加入看板」写成一个投递文件。
//!
//! 插件只写这里，从不直接碰 `board.json`——推进循环（runner）是 `board.json`
//! 的唯一写者，投递与消费由此解耦。文件形状与宿主的
//! `yi-agent-board-ui::inbox`、以及本插件 `board-runner` 的消费端一致：
//! `{ "id", "spec_path", "plan_path" }`。

use std::path::{Path, PathBuf};

/// 投递目录：`<state_dir>/inbox`。
pub fn inbox_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("inbox")
}

/// 一张卡片的投递文件：`<state_dir>/inbox/<id>.json`。
pub fn enqueue_path(state_dir: &Path, id: &str) -> PathBuf {
    inbox_dir(state_dir).join(format!("{id}.json"))
}

/// 写一个投递文件。同 id 覆盖（幂等）；temp + rename 原子替换，
/// 读者永远看不到半个文件。
pub fn deliver_card(state_dir: &Path, id: &str, spec: &str, plan: &str) -> std::io::Result<()> {
    let dir = inbox_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let path = enqueue_path(state_dir, id);
    let body = serde_json::json!({
        "id": id,
        "spec_path": spec,
        "plan_path": plan,
    });
    let text = serde_json::to_string_pretty(&body).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivers_one_json_file_per_card() {
        let dir = tempfile::tempdir().unwrap();
        deliver_card(dir.path(), "card-1", "a.spec.md", "a.plan.md").unwrap();
        let path = dir.path().join("inbox/card-1.json");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["id"], "card-1");
        assert_eq!(value["spec_path"], "a.spec.md");
        assert_eq!(value["plan_path"], "a.plan.md");
    }

    #[test]
    fn delivering_is_idempotent_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        deliver_card(dir.path(), "card-1", "a.spec.md", "a.plan.md").unwrap();
        deliver_card(dir.path(), "card-1", "b.spec.md", "b.plan.md").unwrap();
        let mut names: Vec<_> = std::fs::read_dir(dir.path().join("inbox"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["card-1.json".to_string()]);
        assert!(!dir.path().join("inbox/card-1.json.tmp").exists());
        // 后一次覆盖前一次。
        let text = std::fs::read_to_string(dir.path().join("inbox/card-1.json")).unwrap();
        assert!(text.contains("b.spec.md"));
    }

    #[test]
    fn creates_the_inbox_directory_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!dir.path().join("inbox").exists());
        deliver_card(dir.path(), "card-1", "a", "b").unwrap();
        assert!(dir.path().join("inbox").is_dir());
    }
}
