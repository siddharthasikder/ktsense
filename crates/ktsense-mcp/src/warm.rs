//! A warm engine session held for as long as the MCP server runs, when nothing else holds one.
//!
//! What this does and, more importantly, what it does not do. Tool calls are answered by a child
//! `ktsense` process, not through this session: the process boundary is the seam that keeps this
//! crate off the binary crate, and that does not change here. What a held session buys is the
//! engine's index. `kmp-lsp` builds an index per workspace and caches it on disk; KT-53 measured
//! ktor at 1.04 s cold against 0.72 s warm. A session that stays alive builds that index once, so
//! the children that follow read a settled cache instead of each cold-starting one.
//!
//! So this is the daemon's job done in the MCP server's own process, for the case where no daemon is
//! running. When one is, it is already holding a warm session for the root and a second one would
//! only mean two engines indexing the same tree.
//!
//! Only the tools that wait on the index warm a root. `check_kotlin_syntax` and `find_kotlin_symbol`
//! need the engine but not its index, and making the first `check` after an edit pay a session
//! launch would be a straight regression on the loop that tool exists for.
//!
//! Warming never fails a tool call. It is an optimisation, so a launch that fails or times out is
//! recorded and the call is delegated anyway.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

/// How long the whole warm-up may take before it is abandoned. Generous, because a genuinely cold
/// index on the largest pinned corpus is about a second (KT-53) and a bigger tree is allowed to be
/// slower; finite, because a wedged engine must not hold a tool call open.
pub const DEFAULT_WARM_UP_BOUND: Duration = Duration::from_secs(30);

/// How long to wait for the index to settle once the session is initialized. Waiting is the point:
/// an `initialize` that returns before indexing has finished has warmed nothing.
const INDEX_SETTLE_BOUND: Duration = Duration::from_secs(25);

/// What opening and holding a warm session means. The product implementation launches `kmp-lsp`;
/// tests substitute one that records and sleeps, so the caching behaviour is provable without an
/// engine installed.
pub trait Warmer: Send + Sync {
    /// Opens a session for `root` and keeps it alive until [`Warmer::close`].
    fn open(&self, root: PathBuf) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;

    /// Ends every session this warmer holds.
    fn close(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// What deciding a root came to, which is the fact a test asserts rather than inferring it from a
/// clock, and which an answer reports so a caller can weigh how settled the index was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Warmth {
    /// This root was decided on an earlier call; nothing was done.
    Decided,
    /// A daemon is already holding a warm session for this root.
    LeftToDaemon,
    /// A session was opened and is being held.
    Opened,
    /// Opening one failed or timed out. The message is logged, not returned to the agent.
    Failed,
}

impl Warmth {
    /// The label an answer carries. Deliberately says what the server did, not what the engine's
    /// index is: `already_decided` means an earlier call in this session settled this root and does
    /// not restate which way it went.
    pub fn label(self) -> &'static str {
        match self {
            Warmth::Decided => "already_decided",
            Warmth::LeftToDaemon => "left_to_daemon",
            Warmth::Opened => "opened",
            Warmth::Failed => "failed",
        }
    }
}

/// Holds one warm session per root, deciding each root exactly once.
pub struct WarmEngines {
    warmer: Arc<dyn Warmer>,
    bound: Duration,
    decided: Mutex<HashSet<PathBuf>>,
}

impl WarmEngines {
    pub fn new(warmer: Arc<dyn Warmer>) -> Self {
        Self::within(warmer, DEFAULT_WARM_UP_BOUND)
    }

    pub fn within(warmer: Arc<dyn Warmer>, bound: Duration) -> Self {
        Self {
            warmer,
            bound,
            decided: Mutex::new(HashSet::new()),
        }
    }

    /// Makes sure something holds a warm session for `root`, consulting `daemon_holds` only when
    /// this is the first call for that root and we would otherwise open one ourselves.
    ///
    /// The lock is held for the claim and nothing else. Holding it across the warm-up would serialize
    /// every root behind the first one, for as long as that first warm-up took, and this is awaited
    /// inline in the dispatch path: a second workspace's first index-shaped call would have waited out
    /// a thirty second bound belonging to a workspace it has nothing to do with. The claim is what
    /// makes the decision exclusive, so everything after it can run unlocked.
    pub async fn ensure<F, Fut>(&self, root: &Path, daemon_holds: F) -> Warmth
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = bool>,
    {
        if !self.claim(root).await {
            return Warmth::Decided;
        }
        match tokio::time::timeout(self.bound, self.decide(root, daemon_holds)).await {
            Ok(warmth) => warmth,
            Err(_elapsed) => {
                tracing::warn!(root = %root.display(), bound = ?self.bound, "warm-up timed out");
                Warmth::Failed
            }
        }
    }

    /// Whether this caller is the one that decides `root`, recording that it has been decided in the
    /// same step. Reading and writing have to be one operation here: two callers that each looked
    /// first and inserted afterwards would both believe they were first and open two sessions.
    async fn claim(&self, root: &Path) -> bool {
        self.decided.lock().await.insert(root.to_path_buf())
    }

    /// The decision itself, run unlocked and under the caller's bound. The bound covers the daemon
    /// probe as well as the warm-up, because the probe runs a `status` command as a child process and
    /// nothing else would stop a wedged one holding a tool call open.
    async fn decide<F, Fut>(&self, root: &Path, daemon_holds: F) -> Warmth
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = bool>,
    {
        if daemon_holds().await {
            return Warmth::LeftToDaemon;
        }
        match self.warmer.open(root.to_path_buf()).await {
            Ok(()) => Warmth::Opened,
            Err(reason) => {
                tracing::warn!(root = %root.display(), %reason, "could not hold a warm engine");
                Warmth::Failed
            }
        }
    }

    /// Ends every held session. Called when the server stops serving.
    pub async fn close(&self) {
        self.warmer.close().await;
    }
}

/// Launches `kmp-lsp`, initializes it against the root, waits for its index to settle, and holds
/// the session open.
#[derive(Default)]
pub struct EngineWarmer {
    held: Mutex<Vec<ktsense_lsp::LspClient>>,
}

impl Warmer for EngineWarmer {
    fn open(&self, root: PathBuf) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            let client = ktsense_lsp::launch()
                .await
                .map_err(|error| error.to_string())?;
            let config = ktsense_lsp::InitializeConfig {
                root_uri: format!("file://{}", canonical(&root).display()),
                ignore_patterns: vec!["**/build/**".to_string()],
            };
            client
                .initialize(&config)
                .await
                .map_err(|error| error.to_string())?;
            let mut client = client;
            let settled = ktsense_lsp::wait_for_index(&mut client, INDEX_SETTLE_BOUND).await;
            tracing::info!(root = %root.display(), phase = ?settled.phase, "holding a warm engine");
            self.held.lock().await.push(client);
            Ok(())
        })
    }

    fn close(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            for mut client in self.held.lock().await.drain(..) {
                let _ = client.shutdown().await;
            }
        })
    }
}

fn canonical(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Records how many sessions were asked for, and charges a fixed cost for each.
    struct Counting {
        opens: AtomicUsize,
        closes: AtomicUsize,
        cost: Duration,
        outcome: Result<(), String>,
    }

    impl Counting {
        fn costing(cost: Duration) -> Arc<Self> {
            Arc::new(Self {
                opens: AtomicUsize::new(0),
                closes: AtomicUsize::new(0),
                cost,
                outcome: Ok(()),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                opens: AtomicUsize::new(0),
                closes: AtomicUsize::new(0),
                cost: Duration::ZERO,
                outcome: Err("no engine on this host".to_string()),
            })
        }
    }

    impl Warmer for Counting {
        fn open(
            &self,
            _root: PathBuf,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
            Box::pin(async move {
                self.opens.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(self.cost).await;
                self.outcome.clone()
            })
        }

        fn close(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async move {
                self.closes.fetch_add(1, Ordering::Relaxed);
            })
        }
    }

    async fn never_a_daemon() -> bool {
        false
    }

    #[tokio::test]
    async fn a_root_is_decided_once_a_live_daemon_is_left_alone_and_a_failure_is_not_retried() {
        let warmer = Counting::costing(Duration::ZERO);
        let engines = WarmEngines::new(warmer.clone());
        let held = Path::new("/held");
        let daemoned = Path::new("/daemoned");

        let first = engines.ensure(held, never_a_daemon).await;
        let repeat = engines.ensure(held, never_a_daemon).await;
        let daemon = engines.ensure(daemoned, || async { true }).await;
        let daemon_repeat = engines.ensure(daemoned, never_a_daemon).await;

        let broken = Counting::failing();
        let unavailable = WarmEngines::new(broken.clone());
        let failed = unavailable.ensure(held, never_a_daemon).await;
        let failed_repeat = unavailable.ensure(held, never_a_daemon).await;
        engines.close().await;

        assert_eq!(
            (
                first,
                repeat,
                daemon,
                daemon_repeat,
                failed,
                failed_repeat,
                warmer.opens.load(Ordering::Relaxed),
                broken.opens.load(Ordering::Relaxed),
                warmer.closes.load(Ordering::Relaxed),
            ),
            (
                Warmth::Opened,
                Warmth::Decided,
                Warmth::LeftToDaemon,
                Warmth::Decided,
                Warmth::Failed,
                Warmth::Decided,
                1,
                1,
                1,
            )
        );
    }

    #[tokio::test]
    async fn a_second_root_warms_alongside_the_first_rather_than_queueing_behind_its_warm_up() {
        /// Both roots have to be inside `open` at once before either returns. If the decision were
        /// taken under the lock, the second root would still be waiting for it, the first would wait
        /// here forever, and the bound below turns that deadlock into a readable failure instead of a
        /// hung suite.
        struct Rendezvous {
            gate: tokio::sync::Barrier,
            opens: AtomicUsize,
        }

        impl Warmer for Rendezvous {
            fn open(
                &self,
                _root: PathBuf,
            ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
                Box::pin(async move {
                    self.opens.fetch_add(1, Ordering::Relaxed);
                    self.gate.wait().await;
                    Ok(())
                })
            }

            fn close(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
                Box::pin(async {})
            }
        }

        let warmer = Arc::new(Rendezvous {
            gate: tokio::sync::Barrier::new(2),
            opens: AtomicUsize::new(0),
        });
        let engines = WarmEngines::new(warmer.clone());

        // Generous on purpose: the work inside the bound is two barrier waits, so anything but a
        // regression finishes in microseconds, and four agents share this host.
        let both = tokio::time::timeout(
            Duration::from_secs(5),
            futures_join(
                engines.ensure(Path::new("/alpha"), never_a_daemon),
                engines.ensure(Path::new("/beta"), never_a_daemon),
            ),
        )
        .await;

        assert_eq!(
            (both, warmer.opens.load(Ordering::Relaxed)),
            (Ok((Warmth::Opened, Warmth::Opened)), 2),
            "one root's warm-up must not hold the lock the next root needs to claim"
        );
    }

    /// `tokio::join!` in expression position, so the two futures above read as one value.
    async fn futures_join<A: Future, B: Future>(first: A, second: B) -> (A::Output, B::Output) {
        tokio::join!(first, second)
    }

    #[tokio::test]
    async fn neither_a_wedged_warm_up_nor_a_wedged_probe_outlives_the_bound() {
        struct Hanging;
        impl Warmer for Hanging {
            fn open(
                &self,
                _root: PathBuf,
            ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
                Box::pin(async move {
                    std::future::pending::<()>().await;
                    Ok(())
                })
            }

            fn close(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
                Box::pin(async {})
            }
        }

        let engines = WarmEngines::within(Arc::new(Hanging), Duration::from_millis(50));

        let wedged_engine = engines.ensure(Path::new("/wedged"), never_a_daemon).await;
        let wedged_probe = engines
            .ensure(Path::new("/probe"), std::future::pending)
            .await;

        assert_eq!(
            (wedged_engine, wedged_probe),
            (Warmth::Failed, Warmth::Failed),
            "the bound covers the daemon probe as well as the warm-up"
        );
    }
}
