use std::path::{Path, PathBuf};

/// 插件状态目录：`<workdir>/.yi-agent/board`。`board.json` 与 `inbox/` 都在这里。
pub fn board_state_dir(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("board")
}

/// 投递目录：`<state_dir>/inbox`。
pub fn inbox_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("inbox")
}

/// 一张卡片的投递文件：`<state_dir>/inbox/<id>.json`。
pub fn enqueue_path(state_dir: &Path, id: &str) -> PathBuf {
    inbox_dir(state_dir).join(format!("{id}.json"))
}

/// 把「加入看板」写成一个投递文件。宿主只写这里，从不碰 `board.json`。
///
/// 同 id 覆盖（幂等）；temp + rename 原子替换，读者永远看不到半个文件。
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
