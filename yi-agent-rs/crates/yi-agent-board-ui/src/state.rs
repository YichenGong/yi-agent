use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::view::CardRow;

/// 插件原子写出的看板状态文件。
pub fn board_state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("board.json")
}

/// `board.json` 里我们真正需要的部分。其余字段（`spec_path` / `enqueued_at` /
/// `next_order`）故意不收——控制面不依赖它们，插件便可自由演进。
#[derive(Debug, Deserialize)]
struct RawBoard {
    #[serde(default)]
    cards: Vec<RawCard>,
}

#[derive(Debug, Deserialize)]
struct RawCard {
    id: String,
    plan_path: String,
    state: String,
    #[serde(default)]
    order: i64,
    #[serde(default)]
    workdir: Option<String>,
}

/// 读看板状态并映射成可渲染的卡片行。
///
/// 文件缺失、损坏或字段缺失一律**回退为空列表**：看板空着也好过整个前端崩掉。
/// 卡片按 `order` 升序（与队列顺序一致）。
pub fn load_cards(state_dir: &Path) -> Vec<CardRow> {
    let Ok(text) = std::fs::read_to_string(board_state_path(state_dir)) else {
        return Vec::new();
    };
    let Ok(board) = serde_json::from_str::<RawBoard>(&text) else {
        return Vec::new();
    };
    let mut cards: Vec<RawCard> = board.cards;
    cards.sort_by_key(|card| card.order);
    cards
        .into_iter()
        .map(|card| CardRow {
            id: card.id,
            // 线格式已是 snake_case 小写（见 CardState 的 serde 属性），
            // 这里归一化只为防御手写/异构的 board.json。
            state: card.state.to_ascii_lowercase(),
            progress: None,
            detail: card.workdir.unwrap_or(card.plan_path),
        })
        .collect()
}
