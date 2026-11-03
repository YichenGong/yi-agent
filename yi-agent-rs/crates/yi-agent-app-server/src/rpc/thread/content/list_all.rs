//! `thread/listAll` 与其三个 helper。
//!
//! 从 `rpc/thread/mod.rs` 的 `handle()` 逐字搬出（wave2 C3b）。

use crate::protocol::{RequestEnvelope, RequestId, ThreadStatus};
use crate::rpc::{AppState, Flow};
use crate::server::{collect_pinned, ok_response, thread_summary_json_with_status, write_response};
use crate::workspace_index::WorkspaceIndex;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "thread/listAll" => {
            // 分组两段式,与抽取前逐字等价,只多一层「归属优先」的折拢:
            // ① 按索引顺序(最近在前)为**每个**目录建组——含失效目录
            //   (exists:false、threads 空,不深扫),普通会话留在本目录组;
            // ② 带 `driver_project` 的驱动会话折进它所属的项目组(项目可不在
            //   索引里,则合成一组);只含看板会话的目录(卡片/合并 worktree)
            //   第一段被抑制,不再顶层成组。
            // **不动索引**:目录仍留在 workspaces 里,冷会话定位靠它。
            //
            // 整个应答搬到**独立 task** 里做,而不是在本臂内联等待。列表要扫
            // 每个索引目录(逐条读 meta 文件),目录多、盘慢时这段 IO 会把这
            // 一个主循环 task 整个占住:期间收到的 `turn/interrupt` 只能干等,
            // 用户看到的就是「点了 Stop 没反应」。主 runtime 是多线程的,把
            // 列表挪到别的 worker 后,主循环立刻回到 `select!`。
            //
            // 代价是扫目录期间不能借 `&mut threads`。故先把「每个 thread 此刻
            // 的状态」快照成 `HashMap<String, ThreadStatus>`,任务按快照出 JSON
            // ——除了状态来源,输出与内联版本逐字相同。
            let dirs = state.workspaces.list();
            let statuses: HashMap<String, ThreadStatus> = state
                .threads
                .iter()
                .map(|(tid, s)| {
                    (
                        tid.clone(),
                        *s.status.lock().unwrap_or_else(|p| p.into_inner()),
                    )
                })
                .collect();
            let hub = Arc::clone(state.hub);
            let client = state.client.clone();
            let workspaces = Arc::clone(state.workspaces);
            // 关掉组长时**连懒惰种组长都不该发生**：那是磁盘副作用，不是显示
            // 问题。开关是纯值，必须在搬进 `spawn_blocking` 之前拷好。
            let seed_lead = state.lead_agent_enabled;
            tokio::spawn(async move {
                let reply = tokio::task::spawn_blocking(move || {
                    build_list_all_response(&dirs, &workspaces, &statuses, seed_lead)
                })
                .await;
                let body = match reply {
                    Ok(body) => body,
                    Err(error) => {
                        eprintln!("[app-server] thread/listAll task failed: {error}");
                        json!({ "groups": [], "pinned": [] })
                    }
                };
                let _ = write_response(&hub, &client, ok_response(id, body)).await;
            });
            Ok(Flow::Continue)
        }
        _ => Ok(Flow::Unknown),
    }
}

/// 扫描索引里的每个工作目录，产出 `(目录, metas)`。
///
/// `None` 表示「这个目录不该有内容」：路径不可用，或读取失败（权限、IO 抖动）。
/// 调用方据此把该组留成空组——与改造前「读错误降级为空组」的可见性一致。
///
/// 每个**空**项目目录顺便确保有组长（`ensure_lead_meta`）：侧栏里本就没有任何
/// 条目的项目也应有组长入口。限定「没有任何会话」的理由见 `thread/listAll` 臂内
/// 注释；读失败的目录不写（读不到就不该有副作用）；看板 worktree 绝不种组长
/// （真机上的 9 个幽灵组长正是从这里漏出去的）。
///
/// 这个是**纯阻塞函数**，由 `spawn_blocking` 调用：它只碰磁盘，不碰主循环状态，
/// 因此可以整体挪出 `select!`，避免一次长扫描把 `turn/interrupt` 一起堵住。
// 生产路径由 `build_list_all_response` 调用；`pub(crate)` 是给 `mod.rs` 里的
// `scan_workspace_metas_for_test` 测试钩子用的。
pub(crate) fn scan_workspace_metas(
    dirs: &[String],
    seed_lead: bool,
) -> Vec<(String, Option<Vec<crate::thread_store::ThreadMeta>>)> {
    let mut out = Vec::with_capacity(dirs.len());
    for dir in dirs {
        let path = Path::new(dir);
        if !path.is_dir() {
            out.push((dir.clone(), None));
            continue;
        }
        let mut metas = match crate::thread_store::ThreadStore::new(path).list() {
            Ok(metas) => metas,
            Err(e) => {
                eprintln!("[app-server] thread/listAll failed to list {dir}: {e}");
                out.push((dir.clone(), None));
                continue;
            }
        };
        if seed_lead && metas.is_empty() && !crate::plugin_worktree::is_plugin_worktree(path) {
            if let Err(e) =
                crate::lead::ensure_lead_meta(&crate::thread_store::ThreadStore::new(path), path)
            {
                eprintln!("[app-server] thread/listAll could not ensure a lead for {dir}: {e}");
            }
            // 新落的组长 meta 并入本轮列举结果，否则「本目录的会话」是懒创建
            // **之前**的快照，刚种下的组长在这次响应里就不可见。
            if let Ok(fresh) = crate::thread_store::ThreadStore::new(path).list() {
                metas = fresh;
            }
        }
        out.push((dir.clone(), Some(metas)));
    }
    out
}
/// 构造 `thread/listAll` 的应答体（纯函数，可离开主循环执行）。
///
/// 分组规则与臂内联版本逐字相同，唯一差别是状态来自 `statuses` 快照：
/// 扫描在独立 task 里跑，拿不到 `&mut threads`。
fn build_list_all_response(
    dirs: &[String],
    workspaces: &Arc<WorkspaceIndex>,
    statuses: &HashMap<String, ThreadStatus>,
    seed_lead: bool,
) -> serde_json::Value {
    let summary = |m: &crate::thread_store::ThreadMeta| {
        thread_summary_json_with_status(
            m,
            statuses
                .get(&m.thread_id)
                .copied()
                .unwrap_or(ThreadStatus::Idle),
        )
    };
    // 组顺序:先索引目录(保持最近在前),再追加索引外的新项目组。
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut exists_of: HashMap<String, bool> = HashMap::new();
    let mut plain: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    let mut cards: HashMap<String, Vec<serde_json::Value>> = HashMap::new();

    for (dir, metas) in scan_workspace_metas(dirs, seed_lead) {
        if seen.insert(dir.clone()) {
            order.push(dir.clone());
            exists_of.insert(dir.clone(), Path::new(&dir).is_dir());
        }
        // `None` = 失效目录或读失败：组照留（threads 空），与抽取前一致。
        let Some(metas) = metas else {
            continue;
        };
        let own: Vec<_> = metas
            .iter()
            .filter(|m| m.driver_project.is_none())
            .collect();
        // 看板 worktree(`.worktrees/kanban*`)不是项目:它永不顶层成组,也不贡献
        // 「自己的会话」。真机上这类目录里还躺着一个误落盘的幽灵组长
        // (`role:"lead"`,`driver_project: None`)——若照旧把它算进 `own`,该
        // worktree 就会在侧栏里变成一个带「组长」行的顶层项目组(用户报的现象)。
        let plugin_worktree = crate::plugin_worktree::is_plugin_worktree(Path::new(&dir));
        // 该目录有「自己的」普通会话(或本就为空)才作为顶层组保留;
        // 否则它只是个看板 worktree,归拢后被抑制。
        if !plugin_worktree && (!own.is_empty() || metas.is_empty()) {
            for m in own {
                plain.entry(dir.clone()).or_default().push(summary(m));
            }
        } else {
            // 只含看板会话(或误落盘的幽灵组长):抑制顶层空组(稍后不产出)。
            seen.remove(&dir);
            order.retain(|d| d != &dir);
        }
        // 驱动会话:归入 driver_project 指定的组(可为索引外的项目)。
        for m in metas.iter().filter(|m| m.driver_project.is_some()) {
            let target = m.driver_project.clone().unwrap();
            if seen.insert(target.clone()) {
                order.push(target.clone());
                exists_of.insert(target.clone(), Path::new(&target).is_dir());
            }
            exists_of
                .entry(target.clone())
                .or_insert_with(|| Path::new(&target).is_dir());
            cards.entry(target).or_default().push(summary(m));
        }
    }

    let groups: Vec<serde_json::Value> = order
        .iter()
        .map(|ws| {
            let mut merged = plain.remove(ws).unwrap_or_default();
            if let Some(mut folded) = cards.remove(ws) {
                merged.append(&mut folded);
            }
            // 先收集 id 再 `json!`：`merged` 在 `json!` 里被移动，之后就拿不到了。
            let live_ids: Vec<String> = merged
                .iter()
                .filter_map(|t| t["thread_id"].as_str().map(str::to_string))
                .collect();
            json!({
                "workspace": ws,
                "exists": exists_of.get(ws).copied().unwrap_or(false),
                "threads": merged,
                "groups": groups_json(ws, &live_ids),
            })
        })
        .collect();
    let pinned: Vec<serde_json::Value> = collect_pinned(workspaces).iter().map(summary).collect();
    json!({ "groups": groups, "pinned": pinned })
}
/// 某目录的分组表，供 `thread/listAll` 随目录一起下发。
///
/// **只读**：读路径不写盘。过滤掉的悬空成员要等到下一次写操作才真正从文件里
/// 消失（见 spec「列表」一节）——列表请求是高频路径，不该因为它触发磁盘写。
///
/// `live_ids` 是本次扫描出的、属于该目录的全部 thread id：既保证「成员仍存在」，
/// 也保证「成员仍属于这个目录」（跨目录的脏数据在这里被剔除）。
fn groups_json(workspace: &str, live_ids: &[String]) -> serde_json::Value {
    let live: HashSet<&str> = live_ids.iter().map(String::as_str).collect();
    let mut groups = crate::group_store::load(Path::new(workspace));
    crate::group_store::prune(&mut groups, |id| live.contains(id));
    serde_json::Value::Array(
        groups
            .into_iter()
            .map(|g| {
                json!({
                    "id": g.id,
                    "name": g.name,
                    "description": g.description,
                    "threadIds": g.thread_ids,
                })
            })
            .collect(),
    )
}
