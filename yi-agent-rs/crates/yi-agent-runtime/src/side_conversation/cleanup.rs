//! Per-side cleanup receipts.
//!
//! A side's resources are released asynchronously: the last owner drops its
//! lease and a bounded job deletes the tree. "Dropped" is therefore **not**
//! "released", and a host that removed the resource slot at drop time would
//! report a free slot while a directory still exists — the exact failure that
//! lets repeated open/close escape the eight-slot quota.
//!
//! A receipt is the observable half of that job. It carries no lease (holding one
//! would keep the very thing it is waiting to be deleted alive), no transcript and
//! no paths, so it can outlive the side. `Complete` means the delete really
//! finished; `Failed` keeps the slot for a later retry or startup reclamation.

use std::time::Duration;

use tokio::sync::watch;

use super::{SideError, SideErrorReason, SideResult};

/// How a side's cleanup stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideCleanupState {
    /// The delete has not finished: an owner may still be alive, or the job is
    /// queued or running.
    Pending,
    /// The tree is gone (or never existed).
    Complete,
    /// The delete failed. A stable code, never a path or an OS message that could
    /// carry one.
    Failed { code: String },
}

impl SideCleanupState {
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// A handle on one side's cleanup outcome.
#[derive(Clone)]
pub struct SideCleanupReceipt {
    rx: watch::Receiver<SideCleanupState>,
}

impl SideCleanupReceipt {
    /// Wrap a receiver. Crate-visible so the host can hand out clones from its
    /// slot table without exposing the channel type.
    pub(crate) fn new(rx: watch::Receiver<SideCleanupState>) -> Self {
        Self { rx }
    }

    /// The current state, without waiting.
    pub fn state(&self) -> SideCleanupState {
        self.rx.borrow().clone()
    }

    /// Wait for a terminal state, bounded by `grace`.
    ///
    /// Returns `Ok(())` only on `Complete`. A timeout is reported as a resource
    /// limit error rather than as success, so a caller never releases a slot on a
    /// delete that has not happened.
    pub async fn wait(&mut self, grace: Duration) -> SideResult<()> {
        let mut rx = self.rx.clone();
        let outcome = tokio::time::timeout(grace, async {
            loop {
                match rx.borrow().clone() {
                    SideCleanupState::Complete => return Ok(()),
                    SideCleanupState::Failed { code } => {
                        return Err(SideError::new(SideErrorReason::ResourceLimit, code));
                    }
                    SideCleanupState::Pending => {}
                }
                if rx.changed().await.is_err() {
                    // The sender is gone: nobody will ever publish a terminal
                    // state, so this must not look like a release.
                    return Err(SideError::new(
                        SideErrorReason::ResourceLimit,
                        "cleanup_channel_closed",
                    ));
                }
            }
        })
        .await;

        match outcome {
            Ok(result) => result,
            Err(_) => Err(SideError::new(
                SideErrorReason::ResourceLimit,
                "cleanup_timeout",
            )),
        }
    }
}

impl std::fmt::Debug for SideCleanupReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately never prints a path: a receipt may be logged after the
        // side is gone, and its directory name is a capability hint.
        f.debug_struct("SideCleanupReceipt")
            .field("state", &self.state())
            .finish()
    }
}

/// Create the sender/receiver pair a slot stores.
pub(crate) fn channel() -> (
    watch::Sender<SideCleanupState>,
    watch::Receiver<SideCleanupState>,
) {
    watch::channel(SideCleanupState::Pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_pending_receipt_times_out_instead_of_claiming_success() {
        let (_tx, rx) = channel();
        let mut receipt = SideCleanupReceipt::new(rx);
        assert_eq!(receipt.state(), SideCleanupState::Pending);
        let error = receipt.wait(Duration::from_millis(10)).await.unwrap_err();
        assert_eq!(error.reason, SideErrorReason::ResourceLimit);
        assert_eq!(error.message, "cleanup_timeout");
    }

    #[tokio::test]
    async fn a_receipt_reports_the_terminal_state_it_observes() {
        let (tx, rx) = channel();
        let mut receipt = SideCleanupReceipt::new(rx);
        tx.send(SideCleanupState::Complete).unwrap();
        receipt.wait(Duration::from_secs(1)).await.unwrap();
        assert!(receipt.state().is_complete());
    }

    #[tokio::test]
    async fn a_failed_receipt_is_an_error_not_a_silent_release() {
        let (tx, rx) = channel();
        let mut receipt = SideCleanupReceipt::new(rx);
        tx.send(SideCleanupState::Failed {
            code: "cleanup_failed".into(),
        })
        .unwrap();
        let error = receipt.wait(Duration::from_secs(1)).await.unwrap_err();
        assert_eq!(error.message, "cleanup_failed");
    }

    #[tokio::test]
    async fn a_receipt_whose_sender_vanished_never_reports_success() {
        let (tx, rx) = channel();
        let mut receipt = SideCleanupReceipt::new(rx);
        drop(tx);
        let error = receipt.wait(Duration::from_secs(1)).await.unwrap_err();
        assert_eq!(error.message, "cleanup_channel_closed");
    }
}
