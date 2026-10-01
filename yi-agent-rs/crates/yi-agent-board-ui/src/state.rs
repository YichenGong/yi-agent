use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::view::CardRow;

/// 插件原子写出的看板状态文件。
pub fn board_state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("board.json")
}

/// 读看板状态并映射成可渲染的卡片行。
///
/// 映射规则与 `desktop/src/lib/boardState.ts::parseBoard` **逐条同构**：
/// 两端对同一份 `board.json` 必须得到相同的卡片行。
/// - 文件缺失 / 损坏 / 根不是对象 / 顶层 `cards` 不是数组 → 空列表；
/// - 逐卡校验：缺 `id`（或空串）、`plan_path`、`state` 的卡被**跳过**，其余照常保留；
/// - `state` 归一化为小写；`detail` 取 `workdir`，为空或缺省时回退 `plan_path`；
/// - 按 `order` 升序（同 order 保持文件顺序）。
///
/// 这里刻意用 `serde_json::Value` 手工取值，而不是 derive 一个「严格」的卡片结构体：
/// `serde` 遇到 `Vec` 里的坏元素会让**整段**反序列化失败，与 TS 端「跳过坏卡、留下好卡」
/// 的逐卡语义不一致；手工取值让两端的判定逐条对应。
pub fn load_cards(state_dir: &Path) -> Vec<CardRow> {
    let Ok(text) = std::fs::read_to_string(board_state_path(state_dir)) else {
        return Vec::new();
    };
    // 文件缺失、损坏或形状不符一律**回退为空列表**：看板空着也好过整个前端崩掉。
    parse_cards(&text).unwrap_or_default()
}

fn parse_cards(text: &str) -> Option<Vec<CardRow>> {
    let root: Value = serde_json::from_str(text).ok()?;
    let raw_cards = root.get("cards")?.as_array()?;

    let mut mapped: Vec<(i64, CardRow)> = Vec::new();
    for raw in raw_cards {
        let Some(record) = raw.as_object() else {
            continue;
        };
        let id = record.get("id").and_then(Value::as_str).unwrap_or("");
        let Some(plan_path) = record.get("plan_path").and_then(Value::as_str) else {
            continue;
        };
        let Some(state) = record.get("state").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() || plan_path.is_empty() || state.is_empty() {
            continue;
        }
        let order = record.get("order").and_then(Value::as_i64).unwrap_or(0);
        let workdir = record
            .get("workdir")
            .and_then(Value::as_str)
            .filter(|workdir| !workdir.is_empty());
        mapped.push((
            order,
            CardRow {
                id: id.to_string(),
                // 线格式已是 snake_case 小写（见 CardState 的 serde 属性），
                // 这里归一化只为防御手写/异构的 board.json。
                state: state.to_ascii_lowercase(),
                progress: None,
                detail: workdir.unwrap_or(plan_path).to_string(),
            },
        ));
    }
    // `sort_by_key` 稳定：同 order 保持文件顺序，与 TS 的稳定 `Array.sort` 一致。
    mapped.sort_by_key(|(order, _)| *order);
    Some(mapped.into_iter().map(|(_, card)| card).collect())
}
