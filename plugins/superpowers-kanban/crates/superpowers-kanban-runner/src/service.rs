//! 看板服务：把 board.json 与全局并发槽位收敛到一把锁。
//!
//! 推进循环与查询分派共享同一 `BoardService`，所以 `next_launch` 的
//! 「判名额 → 建 worktree → 置 Launching」相对推进循环是原子的，
//! 不会两张卡抢到同一个槽位。
//!
//! 槽位记账有两处，必须一致：`Board` 里只有 `Running` 占槽，而本服务另外
//! 为每张**已认领但尚未 Running**（即 `Launching`）的卡片持有一个
//! `flock` 租约。`Claiming` 不占槽，所以单靠 `Board::claim_next_launch`
//! 连调两次会认领超过 `limit` 张——`Inner::leases` 正是补这个洞：只有领到
//! 租约才会认领，认领成功就把租约存进 map 一直持有到卡片终态。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Local};
use serde_json::{Value, json};

use superpowers_kanban_core::board::Board;
use superpowers_kanban_core::card::{CardId, CardKind, CardState};

use crate::inbox::{self, InboxOutcome};
use crate::lease::{self, Lease};
use crate::merge::{self, MergeOutcome};
use crate::persist;
use crate::worktree::{ensure_worktree, slugify};

/// 一次成功认领：卡片 id、它的 worktree、以及给会话用的标题。
pub struct LaunchClaim {
    pub card_id: String,
    pub workdir: PathBuf,
    pub title: String,
}

/// 一次合并尝试的结果：卡片 id 与合并结局。
pub struct MergeClaim {
    pub card_id: String,
    pub outcome: crate::merge::MergeOutcome,
}

struct Inner {
    board: Board,
    /// 已认领卡片持有的槽位租约。`Running` 卡片也仍在这里，直到终态释放。
    leases: std::collections::HashMap<CardId, Lease>,
    /// `None` 表示拿不到全局租约目录：`next_launch` 必须报错而不是私自
    /// 造一个目录，否则会和别的进程用两个互不相干的池子、悄悄超配额。
    leases_dir: Option<PathBuf>,
}

pub struct BoardService {
    state_dir: PathBuf,
    project_root: PathBuf,
    inner: Mutex<Inner>,
}

/// worktree 当前 HEAD，作为「有无新提交」的对照基线。非 git 目录返回 `None`。
pub fn effective_head(workdir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl BoardService {
    pub fn new(state_dir: PathBuf, project_root: PathBuf, home: Option<PathBuf>) -> Self {
        let leases_dir = home
            .map(|home| {
                home.join(".yi-agent")
                    .join("superpowers-kanban")
                    .join("leases")
            })
            .or_else(lease::global_leases_dir);
        let board = persist::load_board(&state_dir.join("board.json"));
        Self {
            state_dir,
            project_root,
            inner: Mutex::new(Inner {
                board,
                leases: Default::default(),
                leases_dir,
            }),
        }
    }

    /// 中毒的锁照样用：状态是一份纯数据，`panic` 不会留下半写状态，
    /// 因此毒化不值得让整个推进循环停摆。
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn board_path(&self) -> PathBuf {
        self.state_dir.join("board.json")
    }

    fn save(&self, inner: &Inner) {
        let _ = persist::save_board(&self.board_path(), &inner.board);
    }

    /// 认领下一张排队卡：先拿全局槽位租约，再判名额、建 worktree、置 `Launching`。
    ///
    /// 全程在同一把锁内，且认领与持租约是同一件事的两面，所以同一个
    /// `BoardService` 连续调用不会认领超过 `limit` 张——租约被占住后
    /// `acquire_in` 就再也拿不到新的槽位了。
    pub fn next_launch(
        &self,
        limit: u16,
        _now: DateTime<Local>,
    ) -> Result<Option<LaunchClaim>, String> {
        let mut inner = self.lock();
        let Some(dir) = inner.leases_dir.clone() else {
            return Err("no lease directory".into());
        };
        // 先占槽：拿不到就说明名额已满（含本进程自己已持有的）。
        let Some(lease) = lease::acquire_in(&dir, limit as usize) else {
            return Ok(None);
        };
        // 再认领：队空或无名额时把刚拿到的租约原地丢掉（drop 即释放）。
        let Some(card_id) = inner.board.claim_next_launch(limit) else {
            return Ok(None);
        };
        let branch = format!("kanban/{}", slugify(&card_id));
        let workdir = match ensure_worktree(&self.project_root, &card_id, &branch) {
            Ok(path) => path,
            Err(error) => {
                // 认领即占槽：建 worktree 失败必须把卡片标为 Failed，
                // 否则它会卡在 Launching 永远不再被认领。
                let _ = inner.board.transition(&card_id, CardState::Failed);
                self.save(&inner);
                return Err(format!("worktree for {card_id} failed: {error}"));
            }
        };
        if let Some(base) = effective_head(&workdir) {
            let _ = inner.board.set_base_commit(&card_id, base);
        }
        let _ = inner.board.set_workdir(&card_id, workdir.clone());
        let title = title_for(&inner.board, &card_id);
        inner.leases.insert(card_id.clone(), lease);
        self.save(&inner);
        Ok(Some(LaunchClaim {
            card_id: card_id.0,
            workdir,
            title,
        }))
    }

    /// 启动时补领本进程既有 `Running` 卡片的全局名额。
    ///
    /// 重启后 `BoardService` 是新进程、内存 lease 为空，而 board.json 里可能仍有
    /// `Running` 卡片。`next_launch` 的 `free_slots` 只看 `Running` 计数，但全局池
    /// 是空手起家——别的项目会把本该属于这些在跑卡片的名额抢走。为每张
    /// `Running` 卡片补领一个 lease 记进 `Inner::leases`，全局池的占用才与实际
    /// 在跑数量一致。受 `limit` 与 `acquire_in` 约束：领不到就只做尽力而为
    /// （`limit` 缩小时可能领不全），不 panic。
    pub fn adopt_running_leases(&self, limit: u16) {
        let mut inner = self.lock();
        let Some(dir) = inner.leases_dir.clone() else {
            return;
        };
        let ids: Vec<CardId> = inner
            .board
            .cards()
            .iter()
            .filter(|card| card.state == CardState::Running)
            .map(|card| card.id.clone())
            .collect();
        for id in ids {
            if inner.leases.contains_key(&id) {
                continue;
            }
            match lease::acquire_in(&dir, limit as usize) {
                Some(lease) => {
                    inner.leases.insert(id, lease);
                }
                None => {
                    eprintln!(
                        "superpowers-kanban: {} is running but no slot could be re-adopted",
                        id.0
                    );
                }
            }
        }
    }

    /// 会话已起来：卡片进入 `Running`（从此由 `Running` 记账占槽），并记下 thread id。
    ///
    /// **幂等**：一张已 `Running` 的卡再次被上报（同一会话或追问后的续跑）不算
    /// 非法迁移，仅刷新 thread id。对账新增的 `awaiting_merge`/`needs_you → running`
    /// 回流正靠这里落地（见 2026-10-03 看板状态误判）。
    pub fn mark_running(&self, card_id: &str, thread_id: &str) -> Result<(), String> {
        let mut inner = self.lock();
        let id = CardId::new(card_id);
        let already_running = inner
            .board
            .get(&id)
            .map(|card| card.state == CardState::Running)
            .unwrap_or(false);
        if !already_running {
            inner
                .board
                .transition(&id, CardState::Running)
                .map_err(|error| error.to_string())?;
        }
        inner
            .board
            .set_thread_id(&id, thread_id.to_string())
            .map_err(|error| error.to_string())?;
        self.save(&inner);
        Ok(())
    }

    /// 会话收尾：迁移到某个非终态结果并释放槽位。
    pub fn mark_terminal(
        &self,
        card_id: &str,
        outcome: &str,
        _detail: Option<&str>,
    ) -> Result<(), String> {
        let next = match outcome {
            "awaiting_merge" => CardState::AwaitingMerge,
            "needs_you" => CardState::NeedsYou,
            "failed" => CardState::Failed,
            other => return Err(format!("unknown outcome: {other}")),
        };
        let mut inner = self.lock();
        let id = CardId::new(card_id);
        inner
            .board
            .transition(&id, next)
            .map_err(|error| error.to_string())?;
        inner.leases.remove(&id); // 释放槽位：drop 掉 flock
        // 自动派生：实现卡到达 AwaitingMerge 且偏好开启时，就地排一张配对的合并卡。
        if next == CardState::AwaitingMerge {
            let pref = superpowers_kanban_core::layout::project_preferences_path(&self.state_dir);
            if superpowers_kanban_core::switch::read_bool(&pref, "board_auto_merge") == Some(true) {
                if let Some(card) = inner.board.get(&id).cloned() {
                    if card.kind == CardKind::Implementation {
                        let source = format!("kanban/{}", crate::worktree::slugify(&card.id));
                        let base = crate::merge::default_branch(&self.project_root);
                        let merge_id = inner.board.next_free_merge_id(&source, &base);
                        inner.board.enqueue_merge(
                            merge_id,
                            source,
                            base,
                            Some(id.clone()),
                            chrono::Local::now(),
                        );
                    }
                }
            }
        }
        self.save(&inner);
        Ok(())
    }

    /// 启动没成：把卡片标为 `Failed` 并释放槽位。
    pub fn release(&self, card_id: &str, _detail: &str) -> Result<(), String> {
        let mut inner = self.lock();
        let id = CardId::new(card_id);
        inner
            .board
            .transition(&id, CardState::Failed)
            .map_err(|error| error.to_string())?;
        inner.leases.remove(&id);
        self.save(&inner);
        Ok(())
    }

    /// 启动迁移：把两类「重启后已死」的卡片收干净，启动时调用一次。
    ///
    /// 1. 仍是 `Running` 但 `thread_id` 为空的历史卡片 → `NeedsYou`。这类卡片占用
    ///    着一个槽位，但实际上没有会话在跑（旧版本的记录里没有 thread id），迁移
    ///    它时必须一并把槽位让出来。
    /// 2. 仍是 `Launching` 的僵尸卡 → `Failed`。崩溃若发生在「save(board →
    ///    Launching)」之后、「mark_running」之前，board.json 会留下 `Launching`；
    ///    它既不被本函数（只看 `Running`）回收，也不被 `claim_next_launch`
    ///    （只选 `Queued`）选中，会永远卡住。启动时收成 `Failed` 并释放其可能
    ///    存在的 lease。
    /// 3. 仍是 `Merging` 的僵尸卡 → `NeedsYou`。崩溃若发生在「置 Merging」之后、
    ///    「回写终态」之前，合并可能只做了一半，需人确认，故不放行也不判死。
    pub fn migrate_legacy_running(&self) {
        let mut inner = self.lock();
        let legacy_running: Vec<CardId> = inner
            .board
            .cards()
            .iter()
            .filter(|card| {
                card.state == CardState::Running
                    && card.thread_id.as_deref().unwrap_or("").is_empty()
            })
            .map(|card| card.id.clone())
            .collect();
        for id in legacy_running {
            if inner.board.transition(&id, CardState::NeedsYou).is_ok() {
                inner.leases.remove(&id);
            }
        }
        let launching_zombies: Vec<CardId> = inner
            .board
            .cards()
            .iter()
            .filter(|card| card.state == CardState::Launching)
            .map(|card| card.id.clone())
            .collect();
        for id in launching_zombies {
            if inner.board.transition(&id, CardState::Failed).is_ok() {
                inner.leases.remove(&id);
            }
        }
        // 崩溃若发生在「置 Merging」之后、「回写终态」之前，会留下 Merging 僵尸。
        // 合并可能半途，需人确认，故收成 NeedsYou（不是 Failed）。先尽力 abort。
        let merging: Vec<CardId> = inner
            .board
            .cards()
            .iter()
            .filter(|card| card.state == CardState::Merging)
            .map(|card| card.id.clone())
            .collect();
        for id in merging {
            if let Some(card) = inner.board.get(&id).cloned() {
                if let Some(base) = card.base_ref.as_deref() {
                    if let Ok(wt) = merge::prepare(&self.project_root, base) {
                        let _ = std::process::Command::new("git")
                            .arg("-C")
                            .arg(&wt.path)
                            .args(["merge", "--abort"])
                            .output();
                        merge::cleanup(&wt);
                    }
                }
            }
            if inner.board.transition(&id, CardState::NeedsYou).is_ok() {
                inner.leases.remove(&id);
            }
        }
        self.save(&inner);
    }

    /// 消费 inbox 把投递排进**内存里的同一份看板**并落盘，返回每张投递的结果。
    ///
    /// 推进循环与查询服务共享同一实例，所以这里必须改内存板而不是另 load 一份
    /// `board.json` 再写回——否则会覆盖掉持有 lease 的卡片状态（`Launching`）。
    pub fn consume_inbox(&self, now: DateTime<Local>) -> Vec<InboxOutcome> {
        let mut inner = self.lock();
        let outcomes = inbox::consume(&self.state_dir, &mut inner.board, now);
        self.save(&inner);
        outcomes
    }

    /// 试做下一张合并卡。拿不到每项目合并闸即返回 `None`（本项目已有合并在跑）。
    ///
    /// 全程持 `merge.lock`：认领 → 在 base worktree 里 `git merge --no-ff` → 回写终态。
    /// 合并成功且卡片带 `origin_card` 时，把那张实现卡一并送到 `Done`。
    pub fn merge_next(&self) -> Result<Option<MergeClaim>, String> {
        let Some(_gate) = crate::merge_lock::acquire(&self.state_dir) else {
            return Ok(None);
        };
        let mut inner = self.lock();
        let Some(card_id) = inner.board.claim_next_merge(false) else {
            return Ok(None);
        };
        let card = inner.board.get(&card_id).cloned().expect("just claimed");
        let source = card.source_ref.clone().unwrap_or_default();
        let base = card.base_ref.clone().unwrap_or_default();
        let origin = card.origin_card.clone();

        let finish = |inner: &mut Inner, next: CardState| {
            let _ = inner.board.transition(&card_id, next);
            if next == CardState::Done {
                if let Some(origin) = &origin {
                    let _ = inner.board.transition(origin, CardState::Done);
                }
            }
        };

        if !merge::source_branch_exists(&self.project_root, &source) {
            let detail = format!("source branch '{source}' does not exist");
            finish(&mut inner, CardState::NeedsYou);
            self.save(&inner);
            return Ok(Some(MergeClaim {
                card_id: card_id.0,
                outcome: MergeOutcome::GitError(detail),
            }));
        }
        let base_wt = match merge::prepare(&self.project_root, &base) {
            Ok(wt) => wt,
            Err(detail) => {
                finish(&mut inner, CardState::NeedsYou);
                self.save(&inner);
                return Ok(Some(MergeClaim {
                    card_id: card_id.0,
                    outcome: MergeOutcome::GitError(detail),
                }));
            }
        };
        if merge::is_dirty(&base_wt.path) {
            let detail = format!("base worktree {} is dirty", base_wt.path.display());
            merge::cleanup(&base_wt);
            finish(&mut inner, CardState::NeedsYou);
            self.save(&inner);
            return Ok(Some(MergeClaim {
                card_id: card_id.0,
                outcome: MergeOutcome::GitError(detail),
            }));
        }
        let outcome = merge::run(&base_wt.path, &source, &base);
        merge::cleanup(&base_wt);
        let next = match outcome {
            MergeOutcome::Merged => CardState::Done,
            MergeOutcome::Conflict => CardState::NeedsYou,
            MergeOutcome::GitError(_) => CardState::Failed,
        };
        finish(&mut inner, next);
        self.save(&inner);
        Ok(Some(MergeClaim {
            card_id: card_id.0,
            outcome,
        }))
    }

    /// 看板快照。控制面读的就是这个形状。
    pub fn list(&self) -> Value {
        let inner = self.lock();
        json!({
            "cards": inner.board.cards().iter().map(|card| json!({
                "id": card.id.0,
                "state": card.state,
                "spec_path": card.spec_path,
                "plan_path": card.plan_path,
                "workdir": card.workdir,
                "thread_id": card.thread_id,
                "kind": card.kind,
                "source": card.source_ref,
                "base": card.base_ref,
                "origin_card": card.origin_card.as_ref().map(|id| id.0.clone()),
            })).collect::<Vec<_>>(),
        })
    }

    /// 看板上未被占用的合并卡 id（供 dispatch / CLI 共用）。
    pub fn next_free_merge_id(&self, source: &str, base: &str) -> String {
        let inner = self.lock();
        inner.board.next_free_merge_id(source, base).0
    }

    /// 仅测试用：把某张卡片的 `thread_id` 清成 `None`，模拟旧数据。
    ///
    /// 只搬字段、不碰产品逻辑；走一遍序列化契约而不是给 `Board` 开新的
    /// `&mut Card` 通道（那会为了测试扩大产品 API）。
    #[cfg(test)]
    pub(crate) fn clear_thread_id_for_test(&self, card_id: &str) {
        let mut inner = self.lock();
        let Ok(text) = std::fs::read_to_string(self.board_path()) else {
            return;
        };
        let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        if let Some(cards) = value.get_mut("cards").and_then(Value::as_array_mut) {
            for card in cards {
                if card.get("id").and_then(Value::as_str) == Some(card_id) {
                    card["thread_id"] = Value::Null;
                }
            }
        }
        if let Ok(board) = serde_json::from_value::<Board>(value) {
            inner.board = board;
        }
        self.save(&inner);
    }

    /// 仅测试用：往内存 board 排一张真正的实现卡（带 spec/plan 路径）并落盘。
    ///
    /// 用 `Board::enqueue` 而不是手搓 `Card`，卡片与生产通路同形；派生逻辑只看
    /// `kind`/`id`，但状态通路（Queued → Launching/Running）要求它是真的实现卡。
    #[cfg(test)]
    pub(crate) fn enqueue_impl_card_for_test(&self, id: &str) {
        let mut inner = self.lock();
        inner.board.enqueue(
            CardId::new(id),
            PathBuf::from(format!("{id}.spec.md")),
            PathBuf::from(format!("{id}.plan.md")),
            chrono::Local::now(),
        );
        self.save(&inner);
    }

    /// 仅测试用：直接往内存 board 排一张合并卡并落盘，绕开 inbox 通路。
    #[cfg(test)]
    pub(crate) fn enqueue_merge_card_for_test(
        &self,
        id: &str,
        source: &str,
        base: &str,
        origin: Option<&str>,
    ) {
        let mut inner = self.lock();
        inner.board.enqueue_merge(
            CardId::new(id),
            source.to_string(),
            base.to_string(),
            origin.map(CardId::new),
            chrono::Local::now(),
        );
        self.save(&inner);
    }
}

/// 会话标题：由 spec 文件名派生，如 `a.spec.md` → `看板 · a.spec`。
fn title_for(board: &Board, id: &CardId) -> String {
    let spec = board
        .get(id)
        .map(|card| card.spec_path.clone())
        .unwrap_or_default();
    let stem = spec
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| id.0.clone());
    format!("看板 · {stem}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::path::PathBuf;

    fn at() -> chrono::DateTime<chrono::Local> {
        chrono::Local
            .with_ymd_and_hms(2026, 10, 1, 12, 0, 0)
            .single()
            .unwrap()
    }

    /// 建一个真实的最小 git 仓库，作为 project_root 与 worktree 的来源。
    ///
    /// 一并提交 `.gitignore`（忽略 `.yi-agent/`、`.worktrees/`），与真实项目一致：
    /// 看板的状态目录就落在项目根下的 `.yi-agent/`，若不忽略，项目主检出会被
    /// 自己的运行时状态显示成「脏」，合并前置校验（`merge::is_dirty`）会误判。
    fn project_with_worktree(dir: &std::path::Path) -> PathBuf {
        let out = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git init failed");
        std::fs::write(dir.join("README.md"), "seed\n").unwrap();
        std::fs::write(dir.join(".gitignore"), ".yi-agent/\n.worktrees/\n").unwrap();
        for args in [
            vec!["add", "README.md", ".gitignore"],
            vec![
                "-c",
                "user.email=e@e",
                "-c",
                "user.name=E",
                "commit",
                "-q",
                "-m",
                "seed",
            ],
        ] {
            let out = std::process::Command::new("git")
                .args(&args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        }
        dir.to_path_buf()
    }

    fn service_with_card(dir: &std::path::Path) -> BoardService {
        let state_dir = dir.join(".yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut board = superpowers_kanban_core::board::Board::new();
        board.enqueue(
            superpowers_kanban_core::card::CardId::new("card-1"),
            PathBuf::from("a.spec.md"),
            PathBuf::from("a.plan.md"),
            at(),
        );
        persist::save_board(&state_dir.join("board.json"), &board).unwrap();
        // home 指到临时目录，让 lease 落在隔离目录，不污染真实 HOME。
        BoardService::new(state_dir, dir.to_path_buf(), Some(dir.join("home")))
    }

    #[test]
    fn next_launch_claims_the_head_card_and_creates_its_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);

        let claim = service.next_launch(3, at()).unwrap().expect("a claim");
        assert_eq!(claim.card_id, "card-1");
        assert!(claim.workdir.join(".git").exists(), "worktree was created");
        assert_eq!(claim.title, "看板 · a.spec");

        // 再取一次：没有第二张排队卡 → None。
        assert!(service.next_launch(3, at()).unwrap().is_none());
    }

    #[test]
    fn next_launch_is_gated_by_the_slot_limit() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        assert!(
            service.next_launch(0, at()).unwrap().is_none(),
            "no slot -> no claim"
        );
        // 占用那张卡后，名额用尽。
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        assert!(
            service.next_launch(1, at()).unwrap().is_none(),
            "slot taken"
        );
    }

    #[test]
    fn mark_terminal_releases_the_slot_and_records_the_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service
            .mark_terminal(&claim.card_id, "awaiting_merge", None)
            .unwrap();
        let listed = service.list();
        let card = &listed["cards"][0];
        assert_eq!(card["state"], "awaiting_merge");
        assert_eq!(card["thread_id"], "thread-1");
        // 名额已释放：再放一张排队卡即可启动（此处队空，验证 free_slots 间接由 next_launch None 体现）。
        assert!(service.next_launch(1, at()).unwrap().is_none());
    }

    #[test]
    fn mark_running_is_idempotent_and_revives_a_reconciled_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service
            .mark_terminal(&claim.card_id, "awaiting_merge", None)
            .unwrap();
        // 会话续跑（追问）：卡片必须能翻回 running，且不因重复上报报错。
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        assert_eq!(service.list()["cards"][0]["state"], "running");
    }

    #[test]
    fn release_fails_the_card_and_frees_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service
            .release(&claim.card_id, "thread/start failed")
            .unwrap();
        assert_eq!(service.list()["cards"][0]["state"], "failed");
    }

    #[test]
    fn a_legacy_running_card_without_a_thread_becomes_needs_you() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        // 模拟旧数据：把 thread_id 清成 None（在 BoardService 上加一个
        // `#[cfg(test)] pub(crate) fn clear_thread_id_for_test`，只搬字段，不碰产品逻辑）。
        service.clear_thread_id_for_test("card-1");
        service.migrate_legacy_running();
        assert_eq!(service.list()["cards"][0]["state"], "needs_you");
    }

    /// Step A：重启后进程内存 lease 为空，board 里仍在 `Running` 的卡片必须补领
    /// 全局名额；否则全局池少算了它的占用，别的项目会拿到本该属于它的名额而超发。
    #[test]
    fn adopt_running_leases_reclaims_the_slot_of_a_card_that_is_still_running() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let state_dir = dir.path().join(".yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state_dir).unwrap();
        let leases_dir = dir.path().join("home/.yi-agent/superpowers-kanban/leases");

        // board.json：一张正在跑（有 thread id）、一张排队。
        let mut board = Board::new();
        board.enqueue(
            CardId::new("card-1"),
            PathBuf::from("a.spec.md"),
            PathBuf::from("a.plan.md"),
            at(),
        );
        board.enqueue(
            CardId::new("card-2"),
            PathBuf::from("b.spec.md"),
            PathBuf::from("b.plan.md"),
            at(),
        );
        board
            .transition(&CardId::new("card-1"), CardState::Running)
            .unwrap();
        board
            .set_thread_id(&CardId::new("card-1"), "thread-1".to_string())
            .unwrap();
        persist::save_board(&state_dir.join("board.json"), &board).unwrap();

        let service = BoardService::new(state_dir, project, Some(dir.path().join("home")));
        // 补领之前：全局池是空的，别的领取者能抢到那张在跑卡片本该占的名额。
        assert!(
            lease::acquire_in(&leases_dir, 1).is_some(),
            "restart starts with an empty pool"
        );
        service.adopt_running_leases(1);
        // 补领之后：唯一的名额被在跑的卡片占住。
        assert!(
            lease::acquire_in(&leases_dir, 1).is_none(),
            "the running card's slot is re-adopted"
        );
        assert!(
            service.next_launch(1, at()).unwrap().is_none(),
            "limit=1 and the running card already fills it"
        );
        assert_eq!(
            service.list()["cards"][1]["state"],
            "queued",
            "card-2 must not be claimed while the slot is taken"
        );
    }

    /// Step B：崩溃若发生在 save(board → Launching) 之后、mark_running 之前，
    /// board.json 会留下 `Launching` 僵尸卡；启动迁移必须把它收成 `Failed`，
    /// 否则 `migrate_legacy_running` 只认 `Running`、`claim_next_launch` 只认
    /// `Queued`，没人再回收它。
    #[test]
    fn a_launching_card_left_by_a_crash_becomes_failed_on_startup() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let state_dir = dir.path().join(".yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state_dir).unwrap();

        let mut board = Board::new();
        board.enqueue(
            CardId::new("card-1"),
            PathBuf::from("a.spec.md"),
            PathBuf::from("a.plan.md"),
            at(),
        );
        board
            .transition(&CardId::new("card-1"), CardState::Launching)
            .unwrap();
        persist::save_board(&state_dir.join("board.json"), &board).unwrap();

        let service = BoardService::new(state_dir, project, Some(dir.path().join("home")));
        service.migrate_legacy_running();
        assert_eq!(
            service.list()["cards"][0]["state"],
            "failed",
            "a Launching zombie must be reaped at startup"
        );
    }

    #[test]
    fn a_merge_card_merges_its_source_and_sends_the_origin_card_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        // 造一个 source 分支：feat/x 在 main 之外加一个文件。
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&project)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["checkout", "-qb", "feat/x"]);
        std::fs::write(project.join("x.txt"), "x\n").unwrap();
        git(&["add", "x.txt"]);
        git(&["commit", "-qm", "feat"]);
        git(&["checkout", "-q", "main"]);

        let service = service_with_card(&project);
        // 加一张实现卡（配对的 origin）与一张合并卡。
        service.enqueue_merge_card_for_test("m1", "feat/x", "main", Some("card-1"));
        service.mark_running("card-1", "thread-1").unwrap();
        service
            .mark_terminal("card-1", "awaiting_merge", None)
            .unwrap();

        let claim = service.merge_next().unwrap().expect("a merge claim");
        assert_eq!(claim.card_id, "m1");
        let listed = service.list();
        let cards = listed["cards"].as_array().unwrap();
        let states: std::collections::HashMap<_, _> = cards
            .iter()
            .map(|c| {
                (
                    c["id"].as_str().unwrap().to_string(),
                    c["state"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(states["m1"], "done");
        assert_eq!(
            states["card-1"], "done",
            "merge success drives the origin card to done"
        );
    }

    /// 崩溃若发生在「置 Merging」之后、「回写终态」之前，board.json 会留下 `Merging`
    /// 僵尸卡。合并可能只做了一半，需人确认，故启动迁移收成 `NeedsYou`（不是 `Failed`）。
    #[test]
    fn a_merging_card_left_by_a_crash_becomes_needs_you_on_startup() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let state_dir = dir.path().join(".yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state_dir).unwrap();

        let mut board = Board::new();
        board.enqueue_merge(
            CardId::new("m1"),
            "feat/x".to_string(),
            "main".to_string(),
            Some(CardId::new("card-1")),
            at(),
        );
        board
            .transition(&CardId::new("m1"), CardState::Merging)
            .unwrap();
        persist::save_board(&state_dir.join("board.json"), &board).unwrap();

        let service = BoardService::new(state_dir, project, Some(dir.path().join("home")));
        service.migrate_legacy_running();
        assert_eq!(
            service.list()["cards"][0]["state"],
            "needs_you",
            "a Merging zombie must be reaped for human confirmation at startup"
        );
    }

    #[test]
    fn awaiting_merge_derives_a_merge_card_only_when_the_preference_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        // 默认关：不派生。
        service
            .mark_terminal(&claim.card_id, "awaiting_merge", None)
            .unwrap();
        assert_eq!(service.list()["cards"].as_array().unwrap().len(), 1);

        // 打开开关：再收一张卡时会派生合并卡。
        let pref = superpowers_kanban_core::layout::project_preferences_path(&service.state_dir);
        superpowers_kanban_core::switch::write_bool(&pref, "board_auto_merge", true).unwrap();
        service.enqueue_impl_card_for_test("card-2");
        // 状态机不允许 `Queued -> AwaitingMerge`（唯一的合法通路是
        // Queued → Launching → Running → AwaitingMerge），所以这里按生产路径把
        // card-2 真正启动一次，再收尾；被验证的是 `mark_terminal` 的派生逻辑。
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        assert_eq!(claim.card_id, "card-2");
        service.mark_running(&claim.card_id, "thread-2").unwrap();
        service
            .mark_terminal("card-2", "awaiting_merge", None)
            .unwrap();
        let cards = service.list()["cards"].as_array().unwrap().clone();
        assert!(
            cards
                .iter()
                .any(|c| c["kind"] == "merge" && c["origin_card"] == "card-2"),
            "a merge card must be derived for card-2: {cards:?}"
        );
    }
}
