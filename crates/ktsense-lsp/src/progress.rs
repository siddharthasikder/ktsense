//! Index progress tracking derived from the engine's `$/progress` notification stream.

use lsp_types::{ProgressParams, ProgressParamsValue, WorkDoneProgress};

use crate::client::Notification;

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
}
