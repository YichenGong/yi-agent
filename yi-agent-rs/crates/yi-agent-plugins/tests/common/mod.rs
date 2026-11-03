//! 测试夹具：直接读 `plugins/contracts/fixtures/` 的同一份 JSON。
//!
//! **每个测试二进制都会编译这一份**，而各自只用到其中一个子集
//! （`manifest.rs` 不碰签名夹具，`trust.rs` 不碰 `mutated`）。因此这里
//! 必须允许未使用项：否则加一条 trust 夹具就会让 manifest 的测试编译失败，
//! 而那不是真的有代码没人用。
//!
//! 不复制一份到本 crate：契约 fixture 是**唯一真相**，复制品会在某次
//! 改动后悄悄分叉，而两份各自的自测都会绿。路径从 `CARGO_MANIFEST_DIR`
//! 上溯到仓库根，这样在任何 cwd 下跑测试都对。

#![allow(dead_code)]

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

// ---------------------------------------------------------------------------
// 签名夹具：从契约 fixture 读 envelope，而不是在这里重算一份。
//
// 重算会让「Rust 认为的签名字节」与「Node 参考实现认为的字节」各自自洽地
// 存在，两边自测都绿，直到真实发布才发现不一致。所以这里是**读**，
// 不是造。
// ---------------------------------------------------------------------------

/// 根信封原样字节。
pub fn root_envelope_bytes() -> Vec<u8> {
    fixture_bytes("root-v1.json")
}

/// 发布信封原样字节。
pub fn release_envelope_bytes() -> Vec<u8> {
    fixture_bytes("release-v1.json")
}

/// 固定测试种子生成的密钥对（与 `scripts/plugins/sign-contracts.mjs` 同一种子）。
/// 只是**公开测试身份**，不是生产信任根。
pub const TEST_SEED: u8 = 7;
pub const ROTATED_SEED: u8 = 11;
