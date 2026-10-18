//! yi-agent GUI app-server:JSON-RPC 2.0 over stdio。

pub mod attachments;
pub mod broadcast;
pub mod device_store;
pub(crate) mod git_diff;
pub mod git_diff_tool;
pub mod image_upload;
pub mod lead;
// 本模块的类型是**契约里预留、消费方尚未接线**的(见 spec §6.1 的分期):
// Task 2 的两个组长工具、Task 3 的主循环 `apply_lead_control` 才会用到它们,
// 接线前 clippy 会把整个模块判成 dead_code,故在此放行。
// **Task 2/3 合入后必须删掉这行 allow**——那时模块内已无未接线项,留着会
// 掩盖真实的死代码。
#[allow(dead_code)]
pub(crate) mod lead_control;
pub(crate) mod lead_loop;
pub mod lead_memory;
pub mod mcp_rpc;
pub mod model_rpc;
pub mod onboarding_rpc;
pub mod pair_uri;
pub mod pairing;
pub mod protocol;
pub(crate) mod rpc;
pub mod server;
pub mod session;
pub(crate) mod session_driver;
pub mod settings_store;
pub mod skill_rpc;
/// 跨传输复用的测试夹具（`ws.rs` 的 E2E 与 `mod tests` 共用）。
///
/// 放在 `mod tests` 之外，因为 `#[cfg(test)]` 的 `mod tests` 对其它文件不可见；
/// 这里同样只在 test 构建下存在，避免生产构建出现 dead_code。
#[cfg(test)]
pub(crate) mod test_support;
pub mod theme_tool;
pub mod thread_store;
pub mod translate;
pub mod transport;
pub mod workspace_index;
pub(crate) mod worktree_reclaim;
pub mod ws;

pub use server::run;
// Merged entry for `app-server --listen stdio:// --relay <url>`: the CLI crate
// calls it, so it must be publicly reachable (the underlying core is private).
pub use server::serve_stdio_with_relay;
