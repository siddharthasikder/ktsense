//! Golden snapshot of the catalogue as a client receives it from `tools/list`.
//!
//! This is the text an agent reads to decide which tool to reach for, so it is pinned the way the
//! rendered skeletons are: any change to a name, a description, a requirement marker, an input
//! schema or the shared output schema shows up as a reviewable diff rather than as a silent shift
//! in what the model is told. The eight output schemas are identical by construction, because every
//! tool answers with the same `Answer` shape.

use std::path::PathBuf;
use std::sync::Arc;

use ktsense_mcp::{ExecutableRunner, KtsenseServer};

#[test]
fn tools_list_is_pinned_as_the_client_receives_it() {
    let runner = ExecutableRunner::new(PathBuf::from("ktsense"), PathBuf::from("."));
    let server = KtsenseServer::new(Arc::new(runner));

    let listed = rmcp::model::ListToolsResult::with_all_items(server.listed_tools());

    insta::assert_json_snapshot!("tools_list", listed);
}
