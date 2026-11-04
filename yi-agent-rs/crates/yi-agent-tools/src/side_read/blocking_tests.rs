//! Tests for the bounded side blocking pool.
//!
//! Kept in their own file so the production module stays readable; included as a
//! `#[cfg(test)]` child module of `blocking`.

use super::blocking::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

static LIVE: AtomicUsize = AtomicUsize::new(0);

/// A drop-counter the pool can hold as its `owner`.
struct Owner(Arc<AtomicBool>);

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn owner(flag: &Arc<AtomicBool>) -> Arc<dyn Send + Sync> {
    Arc::new(Owner(Arc::clone(flag)))
}

/// A one-shot gate a blocking job waits on, released by the test.
fn gate() -> Arc<(std::sync::Mutex<bool>, std::sync::Condvar)> {
    Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()))
}

fn open_gate(gate: &(std::sync::Mutex<bool>, std::sync::Condvar)) {
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
}

#[tokio::test]
async fn side_blocking_pool_does_not_run_cancelled_waiter() {
    let pool = SideBlockingPool::new(8, 2, 8, 2);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ran = Arc::new(AtomicBool::new(false));
    let ran_clone = Arc::clone(&ran);
    let result = pool
        .run("s", cancel, Arc::new(()), move || -> std::io::Result<()> {
            ran_clone.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
    assert!(result.is_err());
    // Wait long enough for a queued job to have been picked up: asserting
    // immediately would only prove the caller returned early, not that the
    // work never started.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!ran.load(Ordering::SeqCst), "cancelled work must not start");
}

#[tokio::test]
async fn side_blocking_pool_runs_and_returns_the_value() {
    let pool = SideBlockingPool::new(2, 1, 2, 1);
    let result = pool
        .run("s", CancellationToken::new(), Arc::new(()), || Ok(7u32))
        .await
        .unwrap();
    assert_eq!(result, 7);
    assert_eq!(free_host_admissions(&pool), 4);
}

#[tokio::test]
async fn side_blocking_pool_bounds_threads_by_the_host_limit() {
    // Even with many concurrent jobs, threads never exceed the host cap.
    let pool = Arc::new(SideBlockingPool::new(3, 10, 10, 10));
    let held = gate();
    let peak = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..9 {
        let pool = Arc::clone(&pool);
        let held = Arc::clone(&held);
        let peak = Arc::clone(&peak);
        handles.push(tokio::spawn(async move {
            pool.run("s", CancellationToken::new(), Arc::new(()), move || {
                let current = LIVE.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(current, Ordering::SeqCst);
                let mut done = held.0.lock().unwrap();
                while !*done {
                    done = held.1.wait(done).unwrap();
                }
                LIVE.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            })
            .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        live_threads(&pool) <= 3,
        "threads must stay bounded, saw {}",
        live_threads(&pool)
    );
    assert!(
        peak.load(Ordering::SeqCst) <= 3,
        "concurrency must stay bounded, saw {}",
        peak.load(Ordering::SeqCst)
    );
    open_gate(&held);
    for handle in handles {
        let _ = handle.await;
    }
}

#[tokio::test]
async fn side_blocking_pool_bounds_admissions_per_side() {
    let pool = Arc::new(SideBlockingPool::new(4, 2, 8, 2));
    // 2 running + 2 queued fills one side's capacity.
    let held = gate();
    let mut started = Vec::new();
    for _ in 0..4 {
        let pool = Arc::clone(&pool);
        let held = Arc::clone(&held);
        started.push(tokio::spawn(async move {
            pool.run("s", CancellationToken::new(), Arc::new(()), move || {
                let mut done = held.0.lock().unwrap();
                while !*done {
                    done = held.1.wait(done).unwrap();
                }
                Ok(())
            })
            .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        free_side_admissions(&pool, "s"),
        0,
        "the side must hold all four admissions"
    );

    // A fifth job waits and gives up when cancelled, without taking capacity.
    let cancel = CancellationToken::new();
    let fifth = {
        let pool = Arc::clone(&pool);
        let cancel = cancel.clone();
        tokio::spawn(async move { pool.run("s", cancel, Arc::new(()), || Ok(())).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    assert!(fifth.await.unwrap().is_err());

    open_gate(&held);
    for handle in started {
        let _ = handle.await;
    }
}

#[tokio::test]
async fn side_blocking_pool_close_keeps_the_owner_while_work_runs() {
    // A cancelled waiter must not report the work as finished: the owner is
    // released only when the blocking job actually returns.
    let pool = Arc::new(SideBlockingPool::new(1, 1, 1, 1));
    let owner_dropped = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let held = gate();

    let cancel = CancellationToken::new();
    let task = {
        let pool = Arc::clone(&pool);
        let owner_dropped = Arc::clone(&owner_dropped);
        let started = Arc::clone(&started);
        let held = Arc::clone(&held);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            pool.run("s", cancel, owner(&owner_dropped), move || {
                started.store(true, Ordering::SeqCst);
                let mut done = held.0.lock().unwrap();
                while !*done {
                    done = held.1.wait(done).unwrap();
                }
                Ok(())
            })
            .await
        })
    };

    for _ in 0..200 {
        if started.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(started.load(Ordering::SeqCst), "job must have started");

    // The caller gives up, and the pool is told to close.
    cancel.cancel();
    assert!(task.await.unwrap().is_err());
    let drained = pool.shutdown(Duration::from_millis(50));
    assert!(
        !drained,
        "closing must not claim the blocking work finished"
    );
    assert!(
        !owner_dropped.load(Ordering::SeqCst),
        "the owner must stay alive while the job is still running"
    );

    // Once the work finishes, the owner is released.
    open_gate(&held);
    for _ in 0..200 {
        if owner_dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        owner_dropped.load(Ordering::SeqCst),
        "the owner must be released once the work really ends"
    );
}

#[tokio::test]
async fn side_blocking_pool_stop_cancels_queued_work() {
    let pool = Arc::new(SideBlockingPool::new(1, 1, 4, 4));
    let held = gate();
    let ran = Arc::new(AtomicBool::new(false));

    let blocker = {
        let pool = Arc::clone(&pool);
        let held = Arc::clone(&held);
        tokio::spawn(async move {
            pool.run("s", CancellationToken::new(), Arc::new(()), move || {
                let mut done = held.0.lock().unwrap();
                while !*done {
                    done = held.1.wait(done).unwrap();
                }
                Ok(())
            })
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    let queued = {
        let pool = Arc::clone(&pool);
        let ran = Arc::clone(&ran);
        tokio::spawn(async move {
            pool.run("t", CancellationToken::new(), Arc::new(()), move || {
                ran.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Closing drops the queued job; it must never run.
    assert!(!pool.shutdown(Duration::from_millis(20)));
    let queued_result = queued.await.unwrap();
    assert!(queued_result.is_err() || !ran.load(Ordering::SeqCst));
    open_gate(&held);
    let _ = blocker.await;
    assert!(
        !ran.load(Ordering::SeqCst),
        "queued work must not start after a close"
    );
}

#[tokio::test]
async fn side_blocking_pool_survives_a_panicking_job() {
    let pool = SideBlockingPool::new(2, 1, 2, 1);
    let result = pool
        .run(
            "s",
            CancellationToken::new(),
            Arc::new(()),
            || -> std::io::Result<()> { panic!("boom") },
        )
        .await;
    assert!(result.is_err(), "a panic must surface as a failure");
    // The pool still works.
    let next = pool
        .run("s", CancellationToken::new(), Arc::new(()), || Ok(2u8))
        .await
        .unwrap();
    assert_eq!(next, 2);
}

#[tokio::test]
async fn side_blocking_pool_waiting_is_cancellable() {
    let pool = Arc::new(SideBlockingPool::new(1, 1, 1, 1));
    let held = gate();
    let blocker = {
        let pool = Arc::clone(&pool);
        let held = Arc::clone(&held);
        tokio::spawn(async move {
            pool.run("a", CancellationToken::new(), Arc::new(()), move || {
                let mut done = held.0.lock().unwrap();
                while !*done {
                    done = held.1.wait(done).unwrap();
                }
                Ok(())
            })
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    // This job can only wait: host and side admission are both taken.
    let cancel = CancellationToken::new();
    let waited = {
        let pool = Arc::clone(&pool);
        let cancel = cancel.clone();
        tokio::spawn(async move { pool.run("a", cancel, Arc::new(()), || Ok(())).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let result = waited.await.unwrap();
    assert!(result.is_err(), "a cancelled waiter must give up promptly");

    open_gate(&held);
    let _ = blocker.await;
}
