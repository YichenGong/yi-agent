# Kanban Merge Queue Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make merging something the user asks for. `merge-request` only enqueues a card into a new `MergeQueued` state; the daemon promotes the head of that queue to `Running` (and appends the merge turn) only when the per-project merge lock is free.

**Architecture:** A new `CardState::MergeQueued` plus a `merge_queued_at` timestamp is the whole queue — FIFO by that timestamp, no separate queue file, no persisted ordinal. `merge-request` becomes a pure state write. `merge_cancel` is the only way back out. The daemon's `driver_step` gains one step: take an idle card off the head of that queue, acquire a global session slot, acquire `merge.lock`, set the card `Running`, and hand back one action. `merge.lock.holder` stays the single source of truth for "who is merging", surfaced as a derived `merging` flag.

**Tech Stack:** Rust (workspace `plugins/superpowers-kanban`), serde/serde_json, chrono, tempfile for tests; TypeScript + React + vitest in `desktop/`; markdown skill file.

## Global Constraints

- Work in the worktree `…/.worktrees/kanban-state-by-model` on branch `docs/kanban-state-by-model`. Never commit on `main`.
- Rust toolchain path: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`. Node: `export PATH="/opt/homebrew/bin:$PATH"`.
- Run `cargo fmt --all` in `plugins/superpowers-kanban` before every commit.
- Do not run two `cargo test` invocations concurrently; another worktree may be compiling. Check `ps aux | grep -c "[c]argo"` first.
- Never restore `Merging`, `Launching`, or `Paused` as card states. The merge execution state is `Running`.
- The daemon must never touch git and never decide merge success. Only the model writes outcomes.
- No `Co-Authored-By` trailers in commit messages.
- Card states are exactly: `PreQueued`, `Queued`, `Running`, `NeedsYou`, `AwaitingMerge`, `MergeQueued`, `Done`, `Failed`, `Cancelled`.

---

### Task 1: Add the `MergeQueued` state and the queue timestamp

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card.rs`
- Test: same file (`#[cfg(test)] mod tests` at the bottom)

**Interfaces:**
- Consumes: nothing.
- Produces: `CardState::MergeQueued`; `Card::merge_queued_at: Option<DateTime<Local>>` (serde default); `Card::mark_merge_queued(&mut self, at: DateTime<Local>)`; `Card::clear_merge_queued(&mut self)`. Later tasks call these three names exactly.

- [ ] **Step 1: Write the failing test**

Append inside `mod tests` in `card.rs`:

```rust
    #[test]
    fn a_card_without_a_merge_timestamp_reads_as_not_queued() {
        let json = r#"{"id":"a","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"awaiting_merge","enqueued_at":"2026-10-01T00:00:00+08:00","order":0}"#;
        let card: Card = serde_json::from_str(json).unwrap();
        assert_eq!(
            card.merge_queued_at, None,
            "an old board.json has no merge queue field and must read as None"
        );
    }

    #[test]
    fn a_merge_queue_timestamp_survives_a_round_trip() {
        let json = r#"{"id":"a","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"merge_queued","enqueued_at":"2026-10-01T00:00:00+08:00","order":0}"#;
        let mut card: Card = serde_json::from_str(json).unwrap();
        card.mark_merge_queued(Local.with_ymd_and_hms(2026, 10, 31, 9, 0, 0).unwrap());
        let back: Card = serde_json::from_str(&serde_json::to_string(&card).unwrap()).unwrap();
        assert!(back.merge_queued_at.is_some());
        back.clear_merge_queued();
        assert_eq!(back.merge_queued_at, None);
    }

    #[test]
    fn merge_queued_is_a_distinct_state_that_is_not_terminal() {
        assert_ne!(CardState::MergeQueued, CardState::AwaitingMerge);
        assert!(!CardState::MergeQueued.is_terminal());
        assert!(CardState::AwaitingMerge.can_transition_to(CardState::MergeQueued));
        assert!(CardState::MergeQueued.can_transition_to(CardState::AwaitingMerge));
    }
```

If `Local` / `TimeZone` are not already imported in that test module, add `use chrono::{Local, TimeZone};` at the top of `mod tests`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core --lib card::tests::a_merge_queue_timestamp`
Expected: FAIL — `no variant or associated item named MergeQueued` / `no field merge_queued_at`.

- [ ] **Step 3: Add the state variant**

In `pub enum CardState`, insert after `AwaitingMerge`:

```rust
    /// 用户已确认合并，等本项目的合并名额。只有 `merge-request` 能把卡置到这里，
    /// 也只有它意味着"可以合"。daemon 在名额空时把它推进到 `Running` 并追一回合。
    MergeQueued,
```

- [ ] **Step 4: Add the field and the two helpers**

In `pub struct Card`, after `pub title: Option<String>,`:

```rust
    /// 进入合并队列的时刻。FIFO 的唯一依据；退出队列即清空。
    /// 旧状态文件没有此字段 → `None`。
    #[serde(default)]
    pub merge_queued_at: Option<DateTime<Local>>,
```

In `impl Card`, next to the other mutators:

```rust
    /// 入队：记下时刻，FIFO 靠它排序。
    pub fn mark_merge_queued(&mut self, at: DateTime<Local>) {
        self.merge_queued_at = Some(at);
    }

    /// 退队：时刻一并清掉，免得退队后又被当成"排了很久"的候选。
    pub fn clear_merge_queued(&mut self) {
        self.merge_queued_at = None;
    }
```

Find every `Card { … }` literal in the crate and add `merge_queued_at: None,`. Use `cargo build -p superpowers-kanban-core` to find them.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core`
Expected: PASS, all previous tests still green (108 before this task).

- [ ] **Step 6: Commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-core/src/card.rs
git commit -m "feat(kanban-core): add the MergeQueued state and its queue timestamp"
```

---

### Task 2: `merge-request` only enqueues

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs` (the `merge_request` method, plus the `MergeRequest` enum)
- Test: same file, `mod tests`

**Interfaces:**
- Consumes: `CardState::MergeQueued`, `Card::mark_merge_queued` (Task 1).
- Produces: `MergeRequest::Queued { position: usize }` (replaces `Granted`), `MergeRequest::Denied(String)`, `MergeRequest::Busy { card_id: String }` is **deleted**. `BoardService::merge_request(&self, card_id: &str) -> Result<MergeRequest, String>` keeps its name. `BoardService::merge_queue_position(&self, card_id: &str) -> Option<usize>` returns 0-based position.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn merge_request_only_queues_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let service = awaiting_merge_service(&dir);
        assert_eq!(
            service.merge_request("card-1").unwrap(),
            MergeRequest::Queued { position: 0 }
        );
        assert_eq!(state_of(&service, &CardId::new("card-1")), "mergequeued");
        // Lock is NOT taken by queueing: a second card can still enqueue.
        service.enqueue_card_for_test("card-2");
        service.set_state("card-2", CardState::AwaitingMerge, None).unwrap();
        assert_eq!(
            service.merge_request("card-2").unwrap(),
            MergeRequest::Queued { position: 1 }
        );
    }

    #[test]
    fn merge_request_refuses_a_card_that_is_not_awaiting_merge() {
        let dir = tempfile::tempdir().unwrap();
        let service = awaiting_merge_service(&dir);
        service.set_state("card-1", CardState::Running, None).unwrap();
        let denied = service.merge_request("card-1").unwrap();
        assert!(
            matches!(&denied, MergeRequest::Denied(reason) if reason.contains("running")),
            "the refusal must name the current state: {denied:?}"
        );
    }

    #[test]
    fn merge_request_is_idempotent_and_keeps_the_queue_position() {
        let dir = tempfile::tempdir().unwrap();
        let service = awaiting_merge_service(&dir);
        service.merge_request("card-1").unwrap();
        let again = service.merge_request("card-1").unwrap();
        assert_eq!(again, MergeRequest::Queued { position: 0 });
    }
```

Reuse the existing helper that builds a service with a card parked in `AwaitingMerge` (search for `mark_terminal(&claim.card_id, "awaiting_merge"` and wrap that sequence in a local `awaiting_merge_service(dir)` helper if none exists).

- [ ] **Step 2: Run test to verify it fails**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib merge_request_only_queues`
Expected: FAIL — no variant `Queued`.

- [ ] **Step 3: Rewrite the enum and the method**

```rust
pub enum MergeRequest {
    Queued { position: usize },
    Denied(String),
}
```

```rust
    /// 把卡放进本项目的合并队列。**只改状态，不拿 `merge.lock`**——抢锁是 daemon
    /// 在开跑那一拍的事（见 `promote_merge_queue`）。这样"排队"才有地方存在：
    /// 申请与执行分开，名额忙时卡能一直排在队列里被看见。
    pub fn merge_request(&self, card_id: &str) -> Result<MergeRequest, String> {
        let id = CardId::new(card_id);
        let mut position = 0;
        let outcome = persist::with_board(&self.state_dir, |board| {
            let Some(card) = board.get(&id).cloned() else {
                return Ok(MergeRequest::Denied(format!("unknown card: {card_id}")));
            };
            // 已在队列里：幂等，报告当前名次，不改顺序（`mark_merge_queued` 会
            // 覆写时间戳，那就等于让重复申请把自己排到最后）。
            if card.state == CardState::MergeQueued {
                position = board
                    .cards()
                    .iter()
                    .filter(|other| other.state == CardState::MergeQueued)
                    .filter(|other| other.merge_queued_at < card.merge_queued_at)
                    .count();
                return Ok(MergeRequest::Queued { position });
            }
            // 授权只有一条入口：用户发话 → 模型 `merge-request`。所以卡必须
            // 已经"活儿干完了"（`awaiting_merge`）；任何别的状态都说明调用方
            // 走错了路，报清当前状态让它自己纠正。
            if card.state != CardState::AwaitingMerge {
                return Ok(MergeRequest::Denied(format!(
                    "card {card_id} is {:?}, not awaiting_merge",
                    card.state
                )));
            }
            position = board
                .cards()
                .iter()
                .filter(|other| other.state == CardState::MergeQueued)
                .count();
            board.transition(&id, CardState::MergeQueued).map_err(|e| e.to_string())?;
            if let Some(card) = board.get_mut(&id) {
                card.mark_merge_queued(chrono::Local::now());
            }
            Ok(MergeRequest::Queued { position })
        })?;
        Ok(outcome)
    }
```

If `Board` has no `get_mut`, add one to `board.rs`:

```rust
    pub fn get_mut(&mut self, id: &CardId) -> Option<&mut Card> {
        self.cards.iter_mut().find(|card| &card.id == id)
    }
```

Delete `merge_queue_position` from this task's scope if `position` is computed inline as above; no separate method is needed.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib merge_request_`
Expected: PASS. Existing merge-request tests that asserted `Granted`/`Busy` must be rewritten to the new enum — keep their intent (e.g. "a queued implementation card is refused" stays, "busy while the gate is held" becomes "two cards can both be queued").

- [ ] **Step 5: Commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-runner/src/service.rs crates/superpowers-kanban-core/src/board.rs
git commit -m "refactor(kanban): merge-request only queues a card"
```

---

### Task 3: `merge-cancel` takes a card back out of the queue

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
- Test: same file, `mod tests`

**Interfaces:**
- Consumes: `Card::clear_merge_queued` (Task 1), `MergeRequest` (Task 2).
- Produces: `BoardService::merge_cancel(&self, card_id: &str) -> Result<(), String>`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn merge_cancel_returns_a_queued_card_to_awaiting_merge() {
        let dir = tempfile::tempdir().unwrap();
        let service = awaiting_merge_service(&dir);
        service.merge_request("card-1").unwrap();
        service.merge_cancel("card-1").unwrap();
        assert_eq!(state_of(&service, &CardId::new("card-1")), "awaitingmerge");
        let card = service.list()["cards"][0].clone();
        assert!(card["merge_queued_at"].is_null());
    }

    #[test]
    fn merge_cancel_refuses_a_card_that_is_already_running_the_merge() {
        let dir = tempfile::tempdir().unwrap();
        let service = awaiting_merge_service(&dir);
        service.merge_request("card-1").unwrap();
        // Simulate the daemon having promoted it.
        service.set_state("card-1", CardState::Running, None).unwrap();
        let error = service.merge_cancel("card-1").unwrap_err();
        assert!(error.contains("running"), "{error}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib merge_cancel_`
Expected: FAIL — no method `merge_cancel`.

- [ ] **Step 3: Implement**

```rust
    /// 退队。**只在 `MergeQueued` 上生效**：这个前置条件是它与 `set_state` 的
    /// 全部区别。队列由 daemon 每拍自动推进，若退队走无条件写，就会在"用户反悔"
    /// 与"daemon 跑了下一拍"之间留下竞态——卡可能已被推进到 `Running`，那时再改
    /// 回去会与一个正在跑的合并回合打架。两边对同一张卡判定互斥，就没有这个窗口。
    pub fn merge_cancel(&self, card_id: &str) -> Result<(), String> {
        let id = CardId::new(card_id);
        persist::with_board(&self.state_dir, |board| {
            let Some(card) = board.get(&id).cloned() else {
                return Err(format!("unknown card: {card_id}"));
            };
            if card.state != CardState::MergeQueued {
                return Err(format!(
                    "card {card_id} is {:?}, not merge_queued; there is nothing to cancel",
                    card.state
                ));
            }
            board
                .transition(&id, CardState::AwaitingMerge)
                .map_err(|e| e.to_string())?;
            if let Some(card) = board.get_mut(&id) {
                card.clear_merge_queued();
            }
            Ok(())
        })
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib merge_cancel_`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-runner/src/service.rs
git commit -m "feat(kanban): add merge-cancel to leave the merge queue"
```

---

### Task 4: The daemon promotes the head of the queue

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs` (`driver_step`, new `promote_merge_queue`)
- Test: same file, `mod tests`

**Interfaces:**
- Consumes: `CardState::MergeQueued`, `Card::merge_queued_at`, `merge_lock::acquire_with_holder`, `lease::acquire_in`, `PendingKind::{Append, LaunchForMerge}`, `tick::merge_objective`.
- Produces: `fn promote_merge_queue(&self, inner: &mut Inner, threads: &[ThreadFacts], limit: u16) -> Option<DriverAction>`. `merge_finish` is unchanged (releases the lock only); the model writes the outcome.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn the_queue_head_starts_merging_when_the_gate_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let service = awaiting_merge_service(&dir);
        service.merge_request("card-1").unwrap();
        let action = service.driver_step(&[], &[]).unwrap().expect("a merge turn");
        match action {
            DriverAction::Append { card_id, objective, .. } => {
                assert_eq!(card_id, "card-1");
                assert!(objective.contains("merge"));
            }
            other => panic!("expected an append into the card's own session, got {other:?}"),
        }
        assert_eq!(state_of(&service, &CardId::new("card-1")), "running");
        let holder = crate::merge_lock::read_holder(&service.state_dir).expect("holder");
        assert_eq!(holder.card_id, "card-1");
    }

    #[test]
    fn only_one_merge_starts_per_tick() {
        let dir = tempfile::tempdir().unwrap();
        let service = awaiting_merge_service(&dir);
        service.enqueue_card_for_test("card-2");
        service.set_state("card-2", CardState::AwaitingMerge, None).unwrap();
        service.merge_request("card-1").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        service.merge_request("card-2").unwrap();
        let _ = service.driver_step(&[], &[]).unwrap().expect("first merge");
        assert_eq!(state_of(&service, &CardId::new("card-2")), "mergequeued");
    }

    #[test]
    fn a_held_gate_leaves_the_queue_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let service = awaiting_merge_service(&dir);
        service.merge_request("card-1").unwrap();
        let gate = crate::merge_lock::acquire_with_holder(&service.state_dir, "someone-else");
        assert!(matches!(gate, crate::merge_lock::MergeAttempt::Granted(_)));
        assert!(service.driver_step(&[], &[]).unwrap().is_none());
        assert_eq!(state_of(&service, &CardId::new("card-1")), "mergequeued");
    }

    #[test]
    fn a_busy_session_is_skipped_and_the_next_candidate_starts() {
        let dir = tempfile::tempdir().unwrap();
        let service = awaiting_merge_service(&dir);
        service.enqueue_card_for_test("card-2");
        service.set_state("card-2", CardState::AwaitingMerge, None).unwrap();
        service.merge_request("card-1").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        service.merge_request("card-2").unwrap();
        // card-1's session is busy, so it cannot take a turn now.
        let threads = vec![facts("thread-1", false, true)];
        let action = service.driver_step(&threads, &[]).unwrap().expect("a merge turn");
        match action {
            DriverAction::Append { card_id, .. } => assert_eq!(card_id, "card-2"),
            other => panic!("expected card-2, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib the_queue_head_starts_merging`
Expected: FAIL — `driver_step` returns `None` because nothing promotes `MergeQueued`.

- [ ] **Step 3: Implement `promote_merge_queue`**

Add to `impl BoardService`:

```rust
    /// 队列推进：名额空时把队首那张推进到 `Running` 并交出合并回合。
    ///
    /// **顺序是"先会话名额、后 merge.lock"**（spec §4.5）。反过来会占着合并锁等
    /// 额度，期间本项目所有合并都被堵住，而它们本可以排在后面等着。
    fn promote_merge_queue(
        &self,
        inner: &mut Inner,
        threads: &[ThreadFacts],
        limit: u16,
    ) -> Option<DriverAction> {
        let board = persist::read_board(&self.state_dir);
        // 已经有人在合并：holder 指向任何卡，本拍就不动。锁本身是判定依据，
        // 不另设 `Merging` 状态（spec §8）。
        if crate::merge_lock::read_holder(&self.state_dir).is_some() {
            return None;
        }
        let mut candidates: Vec<Card> = board
            .cards()
            .iter()
            .filter(|card| card.state == CardState::MergeQueued)
            .cloned()
            .collect();
        // FIFO 只看入队时刻；同刻（旧数据没有时间戳）按 order 再按 id，保证确定性。
        candidates.sort_by(|a, b| {
            (a.merge_queued_at, a.order, &a.id).cmp(&(b.merge_queued_at, b.order, &b.id))
        });
        let Some(leases_dir) = inner.leases_dir.clone() else {
            return None;
        };
        for card in candidates {
            // 有会话的卡要等它空闲才追得进去；没有会话的卡（会话早没了）直接新开。
            let thread_id = card.thread_id.clone().filter(|id| !id.is_empty());
            if let Some(thread) = &thread_id {
                if idle_of(threads, thread) != Some(true) {
                    continue;
                }
            }
            // a) 会话名额。拿不到就整拍不推进——名额是全机共享的，换一张卡也一样
            //    拿不到，继续试只会白烧 CPU。
            let Some(lease) = lease::acquire_in(&leases_dir, limit as usize) else {
                return None;
            };
            // b) 合并锁。拿不到说明别的进程刚抢到：把会话名额放回，换下一张。
            let gate = match crate::merge_lock::acquire_with_holder(&self.state_dir, &card.id.0) {
                crate::merge_lock::MergeAttempt::Granted(lock)
                | crate::merge_lock::MergeAttempt::Reclaimed(lock) => lock,
                crate::merge_lock::MergeAttempt::Busy { .. } => {
                    drop(lease);
                    continue;
                }
            };
            let workdir = match crate::merge::prepare(
                &self.project_root,
                card.base_ref
                    .as_deref()
                    .unwrap_or(&crate::merge::default_branch(&self.project_root)),
            ) {
                Ok(tree) => tree.path,
                Err(error) => {
                    eprintln!("superpowers-kanban: cannot prepare {} for merging: {error}", card.id.0);
                    drop(gate);
                    drop(lease);
                    continue;
                }
            };
            // 落到 `Running`：合并本来就是一次会话回合，执行态就是 running。
            if persist::with_board(&self.state_dir, |board| {
                board
                    .transition(&card.id, CardState::Running)
                    .map_err(|e| e.to_string())?;
                if let Some(card) = board.get_mut(&card.id) {
                    card.clear_merge_queued();
                }
                board
                    .set_workdir(&card.id, workdir.clone())
                    .map_err(|e| e.to_string())?;
                Ok(())
            })
            .is_err()
            {
                drop(gate);
                drop(lease);
                continue;
            }
            inner.merge_gate = Some(gate);
            inner.leases.insert(card.id.clone(), lease);
            let action_id = self.mint_action_id();
            let objective = crate::tick::merge_objective(
                card.source_ref.as_deref().unwrap_or_default(),
                card.base_ref.as_deref().unwrap_or_default(),
                &workdir,
            );
            return Some(match thread_id {
                Some(thread_id) => {
                    self.remember_pending(&action_id, &card.id, PendingKind::Append);
                    DriverAction::Append {
                        action_id,
                        card_id: card.id.0.clone(),
                        thread_id,
                        objective,
                    }
                }
                None => {
                    self.remember_pending(&action_id, &card.id, PendingKind::LaunchForMerge);
                    DriverAction::Launch {
                        action_id,
                        driver_project: self.project_root.to_string_lossy().into_owned(),
                        driver_ref: card.id.0.clone(),
                        card_id: card.id.0.clone(),
                        workdir,
                        title: String::new(),
                        objective,
                        env: vec![("SUPERPOWERS_KANBAN_CARD".to_string(), card.id.0.clone())],
                    }
                }
            });
        }
        None
    }
```

The env key is `SUPERPOWERS_KANBAN_CARD` (the same key the launch path already uses).
`set_workdir` lives on `Board`, not `Card` — call `board.set_workdir(&card.id, workdir.clone())`
in the same `with_board` closure rather than through `card`.

- [ ] **Step 4: Call it from `driver_step` before the launch step**

In `driver_step`, after `self.write_observations(threads);` and before the existing launch step:

```rust
        // 4) 合并队列：用户确认过的卡在这里排到队。放在启动之前——合并优先，
        //    且它一拍最多开一个、开完即返回，本拍不会再起新会话。
        let limit = superpowers_kanban_core::calendar::ConcurrencyCalendar::load_preferring_new(
            &self.state_dir,
        )
        .limit_at(chrono::Local::now());
        let mut inner = self.lock();
        if let Some(action) = self.promote_merge_queue(&mut inner, threads, limit) {
            return Ok(Some(action));
        }
        drop(inner);
```

Remove the now-duplicated `let limit = …` from the launch step.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-runner/src/service.rs
git commit -m "feat(kanban): promote the merge queue head when the gate is free"
```

---

### Task 5: Delete the retry ledger and the old merge round

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs`
- Test: same file

**Interfaces:**
- Consumes: Task 4.
- Produces: removes `MAX_MERGE_RETRIES`, `merge_retry_count`, `merge_retries`, `any_merging`, and the `CardState::Queued => card.kind == CardKind::Merge` branch of the old merge round.

- [ ] **Step 1: Delete and re-run the whole suite**

Remove `const MAX_MERGE_RETRIES`, `fn merge_retry_count`, the `merge_retries` field from `Inner`, and `fn any_merging` plus its call sites. Also delete any test that only exercises retry counting (`grep -n "MAX_MERGE_RETRIES\|merge_retry" crates/superpowers-kanban-runner/src/service.rs`).

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib`
Expected: PASS. Nothing fails because Task 4 already replaced the old round; if something does, it was depending on the deleted retry ledger and should be deleted with a note in the commit body.

- [ ] **Step 2: Commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-runner/src/service.rs
git commit -m "refactor(kanban): drop the merge retry ledger"
```

---

### Task 6: Expose the queue and the merging flag

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/service.rs` (`list`)
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs` (`merge_cancel`, `parse_state`)
- Test: both files

**Interfaces:**
- Consumes: `merge_lock::read_holder`, `BoardService::merge_cancel`.
- Produces: `list` card objects gain `merge_queued_at` (RFC3339 or null) and `merging: bool`; IPC method `"merge_cancel"`.

- [ ] **Step 1: Write the failing test**

In `service.rs` tests:

```rust
    #[test]
    fn list_marks_the_card_that_holds_the_merge_gate() {
        let dir = tempfile::tempdir().unwrap();
        let service = awaiting_merge_service(&dir);
        service.merge_request("card-1").unwrap();
        assert_eq!(service.list()["cards"][0]["merging"], false);
        let _ = service.driver_step(&[], &[]).unwrap();
        assert_eq!(service.list()["cards"][0]["merging"], true);
        assert!(service.list()["cards"][0]["merge_queued_at"].is_null());
    }

    #[test]
    fn list_reports_the_queue_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let service = awaiting_merge_service(&dir);
        service.merge_request("card-1").unwrap();
        assert!(service.list()["cards"][0]["merge_queued_at"].is_string());
    }
```

In `dispatch.rs` tests:

```rust
    #[test]
    fn merge_cancel_is_reachable_over_ipc() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path();
        // Seed a card already in the queue.
        let mut board = Board::new();
        board.enqueue(CardId::new("card-1"), "c.spec.md".into(), "c.plan.md".into(), chrono::Local::now());
        board.transition(&CardId::new("card-1"), CardState::AwaitingMerge).unwrap();
        board.transition(&CardId::new("card-1"), CardState::MergeQueued).unwrap();
        persist::save_board(&state.join("board.json"), &board).unwrap();
        dispatch_with_global(state, None, "merge_cancel", &json!({ "card_id": "card-1" })).unwrap();
        let back = persist::load_board(&state.join("board.json"));
        assert_eq!(back.get(&CardId::new("card-1")).unwrap().state, CardState::AwaitingMerge);
    }

    #[test]
    fn parse_state_accepts_merge_queued() {
        assert_eq!(parse_state("merge_queued").unwrap(), CardState::MergeQueued);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib list_marks_the_card`
Expected: FAIL — no `merging` key.

- [ ] **Step 3: Implement**

In `list`, add to each card object:

```rust
                "merge_queued_at": card.merge_queued_at.map(|at| at.to_rfc3339()),
                // 谁在合并由 holder 说话，不是靠一个状态：合并是一次会话回合，
                // 回合期间卡就是 running（spec §4.3）。
                "merging": crate::merge_lock::read_holder(&self.state_dir)
                    .is_some_and(|holder| holder.card_id == card.id.0),
```

Hoist `read_holder` out of the per-card closure:

```rust
        let holder = crate::merge_lock::read_holder(&self.state_dir);
```

then use `holder.as_ref().is_some_and(|h| h.card_id == card.id.0)`.

In `dispatch.rs`, add next to the other merge methods:

```rust
        "merge_cancel" => {
            let card_id = params
                .get("card_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "merge_cancel needs a `card_id`".to_string())?;
            service.merge_cancel(card_id)?;
            Ok(json!({ "ok": true }))
        }
```

and add `"merge_queued" => Ok(CardState::MergeQueued),` to `parse_state`, extending the error message's expected list.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-runner/src/service.rs crates/superpowers-kanban-runner/src/dispatch.rs
git commit -m "feat(kanban): expose the merge queue and who holds the gate"
```

---

### Task 7: CLI `merge-cancel`, `[merging]`, and the objective rewording

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/tick.rs`
- Test: both files

**Interfaces:**
- Consumes: IPC `merge_cancel`, `list.merging`, `list.merge_queued_at`.
- Produces: CLI verb `merge-cancel <card-id>`; `list` lines ending in ` [merging]`; `merge_objective` text that no longer tells the session to call `merge-request`.

- [ ] **Step 1: Write the failing test**

In `main.rs` tests:

```rust
    #[test]
    fn merge_cancel_parses_and_needs_a_card_id() {
        assert!(matches!(
            parse(&["merge-cancel", "card-1"]),
            Ok(Subcommand::MergeCancel { .. })
        ));
        assert!(parse(&["merge-cancel"]).is_err());
    }

    #[test]
    fn the_listing_marks_the_card_that_is_merging() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path();
        let mut board = Board::new();
        board.enqueue(CardId::new("card-1"), "c.spec.md".into(), "c.plan.md".into(), chrono::Local::now());
        board.transition(&CardId::new("card-1"), CardState::Running).unwrap();
        persist::save_board(&state.join("board.json"), &board).unwrap();
        let holder = superpowers_kanban_runner::merge_lock::acquire_with_holder(state, "card-1");
        assert!(matches!(holder, superpowers_kanban_runner::merge_lock::MergeAttempt::Granted(_)));
        let listing = command_list(state, false).unwrap();
        assert!(listing.contains("[merging]"), "{listing}");
    }
```

In `tick.rs` tests:

```rust
    #[test]
    fn the_merge_objective_no_longer_asks_for_the_slot() {
        let text = merge_objective("feat/x", "main", std::path::Path::new("/tmp/tree"));
        assert!(
            !text.contains("merge-request"),
            "the slot is already held when this turn is appended: {text}"
        );
        assert!(text.contains("merge-finish"), "it still has to release the slot");
        assert!(
            !text.contains("Do not run git merge directly"),
            "the slot is already held, so the old instruction would make it try to queue again: {text}"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --bin superpowers-kanban merge_cancel_parses`
Expected: FAIL — unknown verb.

- [ ] **Step 3: Implement the CLI verb**

Add to `enum Subcommand`:

```rust
    MergeCancel { state_dir: PathBuf, card_id: String },
```

Add to `parse`:

```rust
        Some("merge-cancel") => {
            let mut args = args.iter().skip(1);
            let card_id = args
                .next()
                .filter(|id| !id.starts_with("--"))
                .ok_or_else(|| format!("merge-cancel needs a card id\n{USAGE}"))?
                .to_string();
            let rest: Vec<_> = args.collect();
            let state_dir = flag_path(&rest, "--state-dir").unwrap_or_else(default_state_dir);
            Ok(Subcommand::MergeCancel { state_dir, card_id })
        }
```

Add to the dispatch `match`:

```rust
        Subcommand::MergeCancel { state_dir, card_id } => {
            command_merge_cancel(&state_dir, &card_id)
        }
```

and the command:

```rust
/// `merge-cancel`：退队。只在卡仍排着队时有效（spec §4.2）。
fn command_merge_cancel(state_dir: &std::path::Path, card_id: &str) -> Result<String, String> {
    superpowers_kanban_runner::dispatch::dispatch(
        state_dir,
        "merge_cancel",
        &serde_json::json!({ "card_id": card_id }),
    )
    .map_err(|error| error.to_string())?;
    Ok(format!("cancelled: {card_id} is back to awaiting_merge"))
}
```

Update `USAGE` (add `merge-cancel` to the verb list and a line for it) — the skill regression test parses this.

- [ ] **Step 4: Add `[merging]` to the listing**

In `command_list`, inside the `others` loop, next to the `[stale]` marker:

```rust
        let merging = if card_json_merging(state_dir, &card.id.0) {
            " [merging]"
        } else {
            ""
        };
```

Simplest correct implementation: read the holder once before the loop and compare ids, exactly as `service.list()` does:

```rust
    let holder = superpowers_kanban_runner::merge_lock::read_holder(state_dir);
```

then per card:

```rust
        let merging = if holder.as_ref().is_some_and(|h| h.card_id == card.id.0) {
            " [merging]"
        } else {
            ""
        };
        lines.push(format!(
            "{} {:?}{}{}{}{}",
            card.id.0, card.state, where_, archived, stale, merging
        ));
```

- [ ] **Step 5: Reword `merge_objective`**

```rust
pub fn merge_objective(source: &str, base: &str, workdir: &Path) -> String {
    format!(
        "This card is merging the branch {source} into {base}. \
         The project merge slot is already held for you; do not take it again. \
         Merge in {workdir} (that tree has {base} checked out, this session's tree \
         may not), resolve any conflicts in place, and commit the merge. \
         Do not push and do not delete the source branch. \
         Then release the slot with `superpowers-kanban merge-finish <card-id>` and \
         record the outcome with `superpowers-kanban state --self <state>`: `done` when \
         `git merge-base --is-ancestor {source} {base}` succeeds, otherwise \
         `needs_you` with the reason.",
        workdir = workdir.display(),
    )
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`
Expected: PASS. The existing `the_engagement_objective_is_verbatim` and merge-objective assertions must be updated to the new text — update them, do not delete them.

- [ ] **Step 7: Commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
git add crates/superpowers-kanban-runner/src/main.rs crates/superpowers-kanban-runner/src/tick.rs
git commit -m "feat(kanban): add merge-cancel and mark the merging card in list"
```

---

### Task 8: Update the skill

**Files:**
- Modify: `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`
- Test: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs` (`the_skill_*` tests)

**Interfaces:**
- Consumes: the CLI surface from Task 7.
- Produces: a skill that documents every verb and state, including `merge-cancel` and `MergeQueued`.

- [ ] **Step 1: Run the skill regression tests to see what breaks**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --bin superpowers-kanban the_skill`
Expected: FAIL — `merge-cancel` missing from the skill's verb table, `merge_queued` missing from its state list.

- [ ] **Step 2: Edit SKILL.md**

1. Verb table: add a row

```
| `superpowers-kanban merge-cancel <id>` | 不再合并这张卡，退回等待验收 | 用户反悔 |
```

2. State table: add

```
| `merge_queued` | 用户已确认合并，正在等本项目的合并名额（每项目同时只跑一个） |
```

3. Reorder §2b so the user-facing flow reads: 用户说"合并" → 模型 `merge-request`（入队，打印"前面还有 N 个"）→ **不要自己动手**，等 daemon 追一回合进来 → 那一回合里 `merge-finish` → `state --self done`。State explicitly: 入队之前不要 `git merge`，入队之后也不要抢名额，名额在回合开始时已经持有了。

4. `list` 说明里加 `[merging]`：行尾的 `[merging]` 表示这张卡持有本项目的合并名额、正在合并。

5. §2d 判定表加一行：`awaiting_merge` 只表示"活儿干完了"，用户确认合并之后才 `merge-request`。

- [ ] **Step 3: Run tests to verify they pass**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --bin superpowers-kanban the_skill`
Expected: PASS (3 tests).

- [ ] **Step 4: Sync the installed copy and commit**

```bash
cp plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md ~/.yi-agent/skills/superpowers-kanban/SKILL.md
diff -q plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md ~/.yi-agent/skills/superpowers-kanban/SKILL.md
git add plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md
git commit -m "docs(kanban): teach the skill the merge queue and merge-cancel"
```

---

### Task 9: Desktop state mapping and the merging badge

**Files:**
- Modify: `desktop/src/lib/superpowersKanbanState.ts`
- Modify: `desktop/src/components/SuperpowersKanbanCard.tsx`
- Test: `desktop/src/lib/superpowersKanbanState.test.ts`, `desktop/src/components/SuperpowersKanbanCard.test.tsx`

**Interfaces:**
- Consumes: `list` payload with `state: "merge_queued"`, `merging: boolean`, `merge_queued_at`.
- Produces: `BoardCard.merging?: boolean`, `BoardCard.mergeQueuedAt?: string`; a `merge_queued` entry in the state name/color map; a `[合并中]` badge.

- [ ] **Step 1: Write the failing test**

In `superpowersKanbanState.test.ts`:

```ts
  it("reads merge_queued and the merging flag", () => {
    const card = normalizeCard({
      id: "c1",
      state: "merge_queued",
      merging: true,
      merge_queued_at: "2026-10-31T09:00:00+08:00",
      kind: "implementation",
    });
    expect(card.state).toBe("merge_queued");
    expect(card.merging).toBe(true);
    expect(card.mergeQueuedAt).toBe("2026-10-31T09:00:00+08:00");
  });

  it("defaults merging to false when the plugin is older", () => {
    expect(normalizeCard({ id: "c1", state: "running" }).merging).toBe(false);
  });
```

In `SuperpowersKanbanCard.test.tsx`:

```tsx
  it("shows a merging badge while the card holds the gate", () => {
    render(<SuperpowersKanbanCard card={card({ state: "running", merging: true })} />);
    expect(screen.getByLabelText("正在合并")).toBeInTheDocument();
  });
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd desktop && npx vitest run src/lib/superpowersKanbanState.test.ts src/components/SuperpowersKanbanCard.test.tsx`
Expected: FAIL — unknown state / no badge.

- [ ] **Step 3: Implement**

Add `merge_queued` to the state map with a distinct color (use the existing "waiting" hue family, label 等待合并名额), add the two fields to `BoardCard` and `normalizeCard`, and render the badge in `SuperpowersKanbanCard.tsx` mirroring the existing `stale` badge (`aria-label="正在合并"`).

- [ ] **Step 4: Run tests and the type check**

Run: `cd desktop && npx vitest run src/lib/superpowersKanbanState.test.ts src/lib/superpowersKanbanBoard.test.ts src/components/SuperpowersKanbanCard.test.tsx && npx tsc --noEmit`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add desktop/src
git commit -m "feat(desktop): show merge queues and the card that is merging"
```

---

### Task 10: Log the change and verify end to end

**Files:**
- Modify: `docs/project-management/superpowers-kanban.md`
- Modify: `docs/project-management/README.md`

**Interfaces:**
- Consumes: everything above.
- Produces: a feature entry with verifiable criteria, and updated counts.

- [ ] **Step 1: Add the feature entry**

Append to `docs/project-management/superpowers-kanban.md`:

```markdown
- [x] **合并要用户确认，队列串行** — `MergeQueued` 是唯一的合并授权：用户发话 → 模型 `merge-request <id>` 只入队（置 `MergeQueued` + `merge_queued_at`），**不拿锁**；daemon 每拍按入队时刻 FIFO 取队首，先要会话名额再要 `merge.lock`（顺序见 spec §4.5），拿到才置 `Running` 并追一回合。`merge-cancel <id>` 只在 `MergeQueued` 上生效，退回 `awaiting_merge`——消除"用户反悔"与"daemon 推进"之间的竞态。谁在合并由 `merge.lock.holder` 表达，`list` 行尾派生 `[merging]`，不设 `Merging` 状态。删掉自动重试（`MAX_MERGE_RETRIES`）。判据：`cargo test -p superpowers-kanban-runner --lib merge_request_only_queues`、`merge_cancel_`、`the_queue_head_starts_merging_when_the_gate_is_free`、`only_one_merge_starts_per_tick`、`a_held_gate_leaves_the_queue_untouched`
```

- [ ] **Step 2: Update the count**

Increment the `superpowers-kanban` row in `docs/project-management/README.md` (`14 / 14` → `15 / 15`).

- [ ] **Step 3: Run the full suite**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd plugins/superpowers-kanban
cargo test -p superpowers-kanban-core
cargo test -p superpowers-kanban-runner
cd ../../desktop && export PATH="/opt/homebrew/bin:$PATH"
npx vitest run src/lib/superpowersKanban* src/components/SuperpowersKanban*
npx tsc --noEmit
```

Expected: all pass.

- [ ] **Step 4: Commit**

```bash
git add docs/project-management
git commit -m "docs: record the merge queue change"
```
