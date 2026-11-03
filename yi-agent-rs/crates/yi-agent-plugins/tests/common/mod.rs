//! 测试夹具：直接读 `plugins/contracts/fixtures/` 的同一份 JSON。
//!
//! 不复制一份到本 crate：契约 fixture 是**唯一真相**，复制品会在某次
//! 改动后悄悄分叉，而两份各自的自测都会绿。路径从 `CARGO_MANIFEST_DIR`
//! 上溯到仓库根，这样在任何 cwd 下跑测试都对。

use std::path::{Path, PathBuf};

/// 仓库根。`yi-agent-rs/crates/yi-agent-plugins` → `yi-agent-rs` → `crates` → `<repo>`。
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("crate lives two levels below the repo root")
        .to_path_buf()
}

pub fn fixture_path(name: &str) -> PathBuf {
    repo_root()
        .join("plugins")
        .join("contracts")
        .join("fixtures")
        .join(name)
}

pub fn fixture_bytes(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name))
        .unwrap_or_else(|e| panic!("could not read fixture {name}: {e}"))
}

pub fn fixture_json(name: &str) -> serde_json::Value {
    serde_json::from_slice(&fixture_bytes(name))
        .unwrap_or_else(|e| panic!("fixture {name} is not valid JSON: {e}"))
}

/// 把 fixture 改一处后用 `parse` 试一遍。所有负例都走这条路径，
/// 这样「黄金样本本身合法」与「改动的确被拒」是同一次断言的两半。
pub fn mutated(name: &str, change: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let mut value = fixture_json(name);
    change(&mut value);
    serde_json::to_vec(&value).expect("mutated fixture must still serialize")
}
