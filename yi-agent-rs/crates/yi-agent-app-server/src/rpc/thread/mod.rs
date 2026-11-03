//! `thread/*` 域的 RPC 入口。
//!
//! 本文件只做**分组分派**：26 个 method 按语义分给四个子模块，各自的 `owns()`
//! 保证穷尽且互斥（见下方 `owns_coverage` 测试）。臂体本身都在子模块里。
//!
//! - [`lifecycle`]：新建 / 恢复 / 分叉 / 删除 / 清空
//! - [`settings`]：改名 / 模型档位 / 置顶 / 标签 / 已读 / 排序 / 压缩
//! - [`content`]：列表（单目录与跨目录）/ diff 读取 / 视图 / 订阅 / items 补齐
//! - [`groups`]：组长会话与四个分组写操作
//!
//! 臂体从 `server.rs` 的 `serve()` dispatch 逐字搬出后未改逻辑，只做了拆分。

use crate::protocol::{RequestEnvelope, RequestId};
use crate::rpc::{AppState, Flow};

pub(crate) mod content;
pub(crate) mod groups;
pub(crate) mod lifecycle;
pub(crate) mod settings;

/// 处理本域的一个 RPC 请求。
///
/// 返回 [`Flow::Continue`] 表示「本域已应答,主循环回到下一轮」(等价于搬移前臂里
/// 的 `continue`);[`Flow::Unknown`] 表示方法不在本域内,交回 `serve()` 的原始
/// dispatch(走既有的 `method_not_found`),响应形状与搬移前一致。
pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    let method = req.method.as_str();
    // 四组按顺序认领；四个 `owns()` 由 `owns_coverage` 测试保证穷尽且互斥，
    // 所以落到 `Flow::Unknown` 的只可能是本域之外的方法。
    if lifecycle::owns(method) {
        return lifecycle::handle(state, req, id).await;
    }
    if settings::owns(method) {
        return settings::handle(state, req, id).await;
    }
    if content::owns(method) {
        return content::handle(state, req, id).await;
    }
    if groups::owns(method) {
        return groups::handle(state, req, id).await;
    }
    Ok(Flow::Unknown)
}

/// 测试入口：让 `rpc_tests` 能直接驱动扫描与"种组长"这条分支。
///
/// 索引在 `serve` 内部，测试没法注入一个空项目来触发它；而这个函数是纯阻塞
/// 且只碰磁盘的，直接调它比经 RPC 更确定。
#[cfg(test)]
pub(crate) fn scan_workspace_metas_for_test(
    dirs: &[String],
    seed_lead: bool,
) -> Vec<(String, Option<Vec<crate::thread_store::ThreadMeta>>)> {
    crate::rpc::thread::content::list_all::scan_workspace_metas(dirs, seed_lead)
}

#[cfg(test)]
mod owns_coverage {
    use super::{content, groups, lifecycle, settings};

    /// 基线清单由 Task 0 从拆分前的 `thread.rs` 枚举，提交入库。
    /// 它必须是**机器枚举**而非手写：手写清单抓不到「原有 method 被漏掉」。
    #[test]
    fn every_thread_method_is_owned_by_exactly_one_group() {
        let all: Vec<&str> = include_str!("../thread_methods_baseline.txt")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        assert_eq!(all.len(), 26, "基线文件必须恰好 26 行");
        let mut bad = vec![];
        for m in &all {
            let n = [
                lifecycle::owns(m),
                settings::owns(m),
                content::owns(m),
                groups::owns(m),
            ]
            .iter()
            .filter(|x| **x)
            .count();
            if n != 1 {
                bad.push(format!("{m} 被 {n} 组认领"));
            }
        }
        assert!(bad.is_empty(), "未归属或重复归属: {bad:#?}");
    }
}
