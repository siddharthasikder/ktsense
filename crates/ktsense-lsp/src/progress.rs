//! Index progress tracking derived from the engine's `$/progress` notification stream.

use std::time::{Duration, Instant};

use lsp_types::{ProgressParams, ProgressParamsValue, WorkDoneProgress};

use crate::client::{LspClient, Notification};

const PROGRESS_METHOD: &str = "$/progress";

/// Where the engine is in building its index, as told by the work-done progress stream.
///
/// kmp-lsp announces only indexing as work-done progress, so every work-done event advances this
/// single lifecycle regardless of its progress token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexPhase {
    /// No progress has been observed yet.
    #[default]
    Pending,
    /// A work-done `begin` or `report` has arrived without a matching `end`.
    Indexing,
    /// A work-done `end` has arrived: results are complete.
    Ready,
}

impl IndexPhase {
    /// Advances the phase for a single notification, leaving it unchanged for anything that is not
    /// a well-formed `$/progress`.
    pub fn observe(self, notification: &Notification) -> IndexPhase {
        if notification.method != PROGRESS_METHOD {
            return self;
        }
        match serde_json::from_value::<ProgressParams>(notification.params.clone()) {
            Ok(params) => self.advance(params.value),
            Err(_ignored) => self,
        }
    }

    fn advance(self, value: ProgressParamsValue) -> IndexPhase {
        match value {
            ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(_))
            | ProgressParamsValue::WorkDone(WorkDoneProgress::Report(_)) => IndexPhase::Indexing,
            ProgressParamsValue::WorkDone(WorkDoneProgress::End(_)) => IndexPhase::Ready,
        }
    }

    /// Whether answers read now are complete rather than a lower bound.
    pub fn is_ready(self) -> bool {
        matches!(self, IndexPhase::Ready)
    }
}

/// What the waiter saw: the phase reached and how long it watched for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexWait {
    pub phase: IndexPhase,
    pub waited: Duration,
}

/// Folds the engine's notification stream into an [`IndexPhase`] until the index is [`Ready`],
/// the stream closes, or `cap` elapses, whichever comes first. The phase reached is returned either
/// way, so the caller can label its answer `partial` rather than pretend the wait paid off.
///
/// [`Ready`]: IndexPhase::Ready
pub async fn wait_for_index(client: &mut LspClient, cap: Duration) -> IndexWait {
    let started = Instant::now();
    let deadline = tokio::time::sleep(cap);
    tokio::pin!(deadline);
    let mut phase = IndexPhase::default();
    loop {
        tokio::select! {
            notification = client.next_notification() => match notification {
                Some(notification) => {
                    phase = phase.observe(&notification);
                    if phase.is_ready() {
                        break;
                    }
                }
                None => break,
            },
            () = &mut deadline => break,
        }
    }
    IndexWait {
        phase,
        waited: started.elapsed(),
    }
}
