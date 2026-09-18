//! Pure transition tests for [`IndexPhase`] over the `$/progress` notification stream.
//!
//! These need no child process: a [`Notification`] is a value type, so the state machine is folded
//! over hand-built notifications and the observed phases asserted in one composed value.

use ktsense_lsp::{IndexPhase, Notification};
use serde_json::{json, Value};

fn progress(value: Value) -> Notification {
    Notification {
        method: "$/progress".to_string(),
        params: json!({ "token": "indexing", "value": value }),
    }
}

#[test]
fn progress_stream_drives_pending_through_indexing_to_ready() {
    let stream = [
        Notification {
            method: "window/logMessage".to_string(),
            params: json!({ "message": "starting" }),
        },
        progress(json!({ "kind": "begin", "title": "Indexing" })),
        progress(json!({ "kind": "report", "percentage": 40 })),
        progress(json!({ "kind": "end" })),
    ];

    let mut phase = IndexPhase::default();
    let mut observed = vec![phase];
    for notification in &stream {
        phase = phase.observe(notification);
        observed.push(phase);
    }

    assert_eq!(
        observed,
        vec![
            IndexPhase::Pending,
            IndexPhase::Pending,
            IndexPhase::Indexing,
            IndexPhase::Indexing,
            IndexPhase::Ready,
        ]
    );
}

#[test]
fn malformed_progress_is_ignored_and_a_lone_end_completes() {
    let malformed = Notification {
        method: "$/progress".to_string(),
        params: json!({ "nonsense": true }),
    };
    let end = progress(json!({ "kind": "end" }));

    let observed = (
        IndexPhase::Indexing.observe(&malformed),
        IndexPhase::Pending.observe(&end),
    );

    assert_eq!(observed, (IndexPhase::Indexing, IndexPhase::Ready));
}
