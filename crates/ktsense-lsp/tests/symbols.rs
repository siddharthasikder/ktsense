//! Process-level tests for the symbol resolver, driving the KT-15 `fake_lsp` binary in `find`
//! command mode. The fake replays whatever `FAKE_CMD_STDOUT`/`FAKE_CMD_STDERR`/`FAKE_CMD_EXIT`
//! hold and hangs on `FAKE_CMD_HANG`, so a unique match, several exact matches, a name that
//! matches nothing, malformed engine output, and a wedged engine are all reproduced without a real
//! `kmp-lsp` install. Those env vars are process-global, so every case runs under a shared guard
//! held across the invocation.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ktsense_lsp::{PassthroughError, Resolution, SymbolCandidate, SymbolResolver};
use tokio::sync::Mutex;

static ENV_GUARD: Mutex<()> = Mutex::const_new(());

fn fake_lsp() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_lsp"))
}

async fn find_replay(
    query: &str,
    stdout: &str,
    stderr: &str,
    exit: i32,
) -> Result<Vec<SymbolCandidate>, PassthroughError> {
    let guard = ENV_GUARD.lock().await;
    std::env::set_var("FAKE_CMD_STDOUT", stdout);
    std::env::set_var("FAKE_CMD_STDERR", stderr);
    std::env::set_var("FAKE_CMD_EXIT", exit.to_string());
    let result = SymbolResolver::new(&fake_lsp(), Path::new("/repo"))
        .find(query)
        .await;
    std::env::remove_var("FAKE_CMD_STDOUT");
    std::env::remove_var("FAKE_CMD_STDERR");
    std::env::remove_var("FAKE_CMD_EXIT");
    drop(guard);
    result
}

fn candidate(name: &str, file: &str, line: u32, col: u32) -> SymbolCandidate {
    SymbolCandidate {
        name: name.to_string(),
        file: file.to_string(),
        line,
        col,
    }
}

#[tokio::test]
async fn a_unique_match_resolves_to_one_candidate_and_a_position() {
    let candidates = find_replay(
        "OrderRepository",
        r#"[{"file":"/repo/core/OrderRepository.kt","line":3,"col":1,"name":"OrderRepository"}]"#,
        "",
        0,
    )
    .await
    .expect("unique match");
    let resolution = Resolution::classify(candidates.clone());
    let position = candidates[0].to_file_position();

    let observed = (candidates, resolution, position.line, position.character);
    assert_eq!(
        observed,
        (
            vec![candidate(
                "OrderRepository",
                "/repo/core/OrderRepository.kt",
                3,
                1
            )],
            Resolution::Unique(candidate(
                "OrderRepository",
                "/repo/core/OrderRepository.kt",
                3,
                1
            )),
            2,
            0,
        )
    );
}

#[tokio::test]
async fn several_exact_matches_come_back_ambiguous_in_engine_order() {
    let candidates = find_replay(
        "save",
        r#"[{"file":"/repo/core/OrderRepository.kt","line":4,"col":5,"name":"save"},
           {"file":"/repo/db/JdbcOrderRepository.kt","line":8,"col":14,"name":"save"},
           {"file":"/repo/db/InMemoryOrderRepository.kt","line":13,"col":14,"name":"save"}]"#,
        "",
        0,
    )
    .await
    .expect("several matches");

    assert_eq!(
        Resolution::classify(candidates),
        Resolution::Ambiguous(vec![
            candidate("save", "/repo/core/OrderRepository.kt", 4, 5),
            candidate("save", "/repo/db/JdbcOrderRepository.kt", 8, 14),
            candidate("save", "/repo/db/InMemoryOrderRepository.kt", 13, 14),
        ])
    );
}

#[tokio::test]
async fn a_name_matching_nothing_is_an_empty_result_not_an_error() {
    let candidates = find_replay("NoSuchSymbol", "", "", 1)
        .await
        .expect("empty, not an error");

    assert_eq!(
        (candidates.clone(), Resolution::classify(candidates)),
        (Vec::new(), Resolution::None)
    );
}

#[tokio::test]
async fn malformed_engine_output_is_diagnosed_as_a_failure() {
    let observed = find_replay("save", "[INFO kmp_lsp] indexing\nthis is not json", "", 0).await;
    assert!(
        matches!(observed, Err(PassthroughError::Unparseable { .. })),
        "was {observed:?}"
    );
}

#[tokio::test]
async fn a_hanging_engine_is_bounded_and_the_child_is_not_left_running() {
    let guard = ENV_GUARD.lock().await;
    std::env::set_var("FAKE_CMD_HANG", "1");
    let binary = fake_lsp();
    let started = Instant::now();
    let result = SymbolResolver::within(&binary, Path::new("/repo"), Duration::from_millis(300))
        .find("save")
        .await;
    let elapsed = started.elapsed();
    std::env::remove_var("FAKE_CMD_HANG");
    drop(guard);

    let observed = (
        matches!(result, Err(PassthroughError::Timeout { .. })),
        elapsed < Duration::from_secs(2),
    );
    assert_eq!(
        observed,
        (true, true),
        "result {result:?} after {elapsed:?}"
    );
}

#[cfg(feature = "real-lsp")]
mod real {
    use std::path::{Path, PathBuf};

    use ktsense_lsp::{resolve_symbol, Resolution};

    fn multi_module() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/multi-module")
    }

    /// The pinned engine's `find --json` against the fixture: `save` is declared three times (the
    /// interface method and its two overrides), a name declared nowhere resolves to nothing, and
    /// every reported file is an absolute path the follow-up LSP request can address.
    #[tokio::test]
    async fn the_real_engine_resolves_save_to_three_declarations_and_an_unknown_name_to_none() {
        let root = multi_module();

        let save = resolve_symbol(&root, "save").await.expect("find save");
        let missing = resolve_symbol(&root, "ZzzNope").await.expect("find missing");

        let observed = match &save {
            Resolution::Ambiguous(candidates) => (
                candidates.len(),
                candidates
                    .iter()
                    .all(|candidate| Path::new(&candidate.file).is_absolute()),
                candidates
                    .iter()
                    .filter(|candidate| candidate.file.ends_with("shop/order/OrderRepository.kt"))
                    .map(|candidate| (candidate.line, candidate.col))
                    .collect::<Vec<_>>(),
                matches!(missing, Resolution::None),
            ),
            other => panic!("expected an ambiguous resolution, got {other:?}"),
        };
        assert_eq!(observed, (3, true, vec![(4, 9)], true));
    }
}
