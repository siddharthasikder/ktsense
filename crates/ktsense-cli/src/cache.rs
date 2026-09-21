//! CLI-side parsed-skeleton cache for the warm daemon.
//!
//! `ktsense-daemon` may not depend on `ktsense-syntax` by layering, and `ktsense-core` is pure, so
//! the only place a syntax skeleton cache can live is the CLI adapter that already owns the routed
//! `outline` and `deps` bodies. This is that cache. It stores the `FileSkeleton` values the syntax
//! adapter produced, keyed by absolute path, alongside a content fingerprint, so repeated `outline`
//! and `deps` requests reuse the parse instead of re-running tree-sitter, and `deps` reuses the very
//! per-file parses `outline` populated.
//!
//! A cached answer must be byte-identical to the in-process one, so [`SkeletonCache::outline`] and
//! [`SkeletonCache::deps`] mirror `crate::outline` and `crate::deps` exactly, swapping only the
//! per-file parse for a fingerprint-guarded cache lookup. The rendering, traversal and graph code is
//! the same code the fresh path calls, so the two cannot drift. A file with a localized parse error
//! is recovered and marked partial exactly as the in-process path recovers it (KT-52a); the cache
//! stores that partial skeleton like any other and its partial marker rides through unchanged.
//!
//! Invalidation is a content fingerprint, not a timestamp or a size: an editor that rewrites a file
//! to the same length within the same coarse mtime tick still changes the fingerprint, so the next
//! request re-parses. There is no watcher thread to leak; a stale entry is caught at request time by
//! the fingerprint mismatch. Only recovered skeletons are cached, so a read or extraction failure is
//! never remembered as an answer.

use std::collections::HashMap;
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use ktsense_core::{build_import_graph, DepLevel, FileSkeleton, RenderOptions};

use crate::{CommandError, Format};

/// A parsed-skeleton cache shared across a daemon's lifetime.
#[derive(Default)]
pub(crate) struct SkeletonCache {
    entries: Mutex<HashMap<std::path::PathBuf, CacheEntry>>,
    #[cfg(test)]
    parses: AtomicU64,
}

struct CacheEntry {
    fingerprint: u64,
    skeleton: FileSkeleton,
}

impl SkeletonCache {
    /// Outlines `file` from the cache, re-parsing only when the file's content changed. Mirrors
    /// `crate::outline`: the same read error, the same recovery-or-rejection from `extract`, and the
    /// same rendering, so a routed answer equals the in-process one byte for byte, partial or not.
    pub(crate) fn outline(
        &self,
        root: &Path,
        file: &Path,
        format: Format,
        options: &RenderOptions,
    ) -> Result<String, CommandError> {
        let source =
            std::fs::read_to_string(file).map_err(|error| CommandError::read(file, &error))?;
        let skeleton = self.resolve(root, file, &source)?;
        crate::present(&skeleton, format, options)
    }

    /// Builds the dependency graph from the cache. Traversal runs fresh every request, so an added
    /// or removed Kotlin file is reflected immediately; each surviving file reuses its cached parse
    /// when its content is unchanged. Mirrors `crate::deps` so the rendered graph is identical.
    pub(crate) fn deps(
        &self,
        root: &Path,
        level: DepLevel,
        format: Format,
    ) -> Result<String, CommandError> {
        let files = crate::collect_kotlin_files(root)?;
        let skeletons: Vec<FileSkeleton> = files
            .iter()
            .filter_map(|path| self.skeleton_for_deps(root, path))
            .collect();
        let graph = build_import_graph(&skeletons, level);
        crate::present_deps(&graph, format)
    }

    /// A malformed or unreadable file contributes no honest edges, so it is skipped rather than
    /// aborting the graph, matching `crate::skeleton_for_deps`. A partial skeleton is kept: its
    /// import edges parsed and are as trustworthy as a complete file's, so the graph consumes it.
    fn skeleton_for_deps(&self, root: &Path, path: &Path) -> Option<FileSkeleton> {
        let source = std::fs::read_to_string(path).ok()?;
        self.resolve(root, path, &source).ok()
    }

    fn resolve(
        &self,
        root: &Path,
        file: &Path,
        source: &str,
    ) -> Result<FileSkeleton, CommandError> {
        let fingerprint = fingerprint(source);
        if let Some(skeleton) = self.hit(file, fingerprint) {
            return Ok(skeleton);
        }
        let skeleton = parse_fresh(root, file, source);
        #[cfg(test)]
        self.parses.fetch_add(1, Ordering::Relaxed);
        if let Ok(skeleton) = &skeleton {
            self.store(file, fingerprint, skeleton.clone());
        }
        skeleton
    }

    fn hit(&self, file: &Path, fingerprint: u64) -> Option<FileSkeleton> {
        let entries = self.entries.lock().expect("cache lock is never poisoned");
        entries
            .get(file)
            .filter(|entry| entry.fingerprint == fingerprint)
            .map(|entry| entry.skeleton.clone())
    }

    fn store(&self, file: &Path, fingerprint: u64, skeleton: FileSkeleton) {
        let mut entries = self.entries.lock().expect("cache lock is never poisoned");
        entries.insert(
            file.to_path_buf(),
            CacheEntry {
                fingerprint,
                skeleton,
            },
        );
    }

    #[cfg(test)]
    fn parse_count(&self) -> u64 {
        self.parses.load(Ordering::Relaxed)
    }
}

/// Parses and extracts one file exactly as `crate::outline` does: a recovered skeleton (complete or
/// partial) on success, and the same `unparseable` rejection when the tree errors and nothing
/// survives, so the routed answer matches the in-process one.
fn parse_fresh(root: &Path, file: &Path, source: &str) -> Result<FileSkeleton, CommandError> {
    ktsense_syntax::extract(crate::normalized_path(root, file), source)
        .map_err(|_| CommandError::unparseable(file))
}

/// FNV-1a over the file's bytes. A content fingerprint catches an equal-size rewrite and a rewrite
/// within one coarse timestamp tick, both of which an mtime or size check would miss, and it needs
/// no new dependency, matching the socket-key hashing already in the daemon crate.
fn fingerprint(source: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in source.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Fixture {
        _root: tempfile::TempDir,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("temp root");
            let path = root.path().to_path_buf();
            Self {
                _root: root,
                root: path,
            }
        }

        fn write(&self, relative: &str, source: &str) -> PathBuf {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().expect("file has a parent"))
                .expect("create dirs");
            std::fs::write(&path, source).expect("write source");
            path
        }

        fn remove(&self, relative: &str) {
            std::fs::remove_file(self.root.join(relative)).expect("remove source");
        }
    }

    fn source_named(function: &str) -> String {
        format!("package shop.order\n\nclass Ledger {{\n    fun {function}(): Int = 0\n}}\n")
    }

    #[test]
    fn repeated_outline_reuses_the_cached_parse_until_the_content_changes() {
        let fixture = Fixture::new();
        let file = fixture.write("Ledger.kt", &source_named("balance"));
        let cache = SkeletonCache::default();
        let options = RenderOptions::default();

        let first = cache.outline(&fixture.root, &file, Format::Md, &options);
        let parses_after_first = cache.parse_count();
        let second = cache.outline(&fixture.root, &file, Format::Md, &options);
        let parses_after_repeat = cache.parse_count();

        fixture.write("Ledger.kt", &source_named("balances"));
        let after_edit = cache
            .outline(&fixture.root, &file, Format::Md, &options)
            .expect("edited outline");
        let parses_after_edit = cache.parse_count();
        let in_process =
            crate::outline(&fixture.root, &file, Format::Md, &options).expect("in-process outline");

        let first = first.expect("first outline");
        assert_eq!(
            (
                first == second.expect("second outline"),
                parses_after_first,
                parses_after_repeat,
                first.contains("balance"),
                first.contains("balances"),
                after_edit == in_process,
                after_edit.contains("balances"),
                parses_after_edit,
            ),
            (true, 1, 1, true, false, true, true, 2),
        );
    }

    #[test]
    fn a_same_size_rewrite_is_observed_rather_than_served_stale() {
        let fixture = Fixture::new();
        let file = fixture.write("Ledger.kt", &source_named("credit"));
        let cache = SkeletonCache::default();
        let options = RenderOptions::default();

        let before = cache.outline(&fixture.root, &file, Format::Md, &options);
        fixture.write("Ledger.kt", &source_named("refund"));
        let after = cache.outline(&fixture.root, &file, Format::Md, &options);

        let before = before.expect("outline before");
        let after = after.expect("outline after");
        assert_eq!(
            (
                before.len() == after.len(),
                before.contains("credit"),
                after.contains("refund"),
                after.contains("credit"),
                cache.parse_count(),
            ),
            (true, true, true, false, 2),
        );
    }

    #[test]
    fn deps_reuses_cached_parses_and_tracks_added_and_removed_files() {
        let fixture = Fixture::new();
        fixture.write(
            "core/Order.kt",
            "package shop.order\n\ndata class Order(val id: Int)\n",
        );
        fixture.write(
            "core/Repository.kt",
            "package shop.order\n\ninterface Repository\n",
        );
        let cache = SkeletonCache::default();

        let first = node_count(cache.deps(&fixture.root, DepLevel::Package, Format::Json));
        let parses_after_first = cache.parse_count();
        let repeat = node_count(cache.deps(&fixture.root, DepLevel::Package, Format::Json));
        let parses_after_repeat = cache.parse_count();

        fixture.write(
            "app/App.kt",
            "package shop.app\n\nimport shop.order.Order\n\nclass App\n",
        );
        let after_add = cache
            .deps(&fixture.root, DepLevel::Package, Format::Json)
            .expect("deps after add");
        let parses_after_add = cache.parse_count();

        fixture.remove("app/App.kt");
        let after_remove = cache
            .deps(&fixture.root, DepLevel::Package, Format::Md)
            .expect("deps after remove");
        let in_process_after_remove =
            crate::deps(&fixture.root, DepLevel::Package, Format::Md).expect("in-process deps");

        assert_eq!(
            (
                first,
                parses_after_first,
                repeat,
                parses_after_repeat,
                after_add.contains("shop.app"),
                node_count(Ok(after_add)),
                parses_after_add,
                after_remove == in_process_after_remove,
                after_remove.contains("shop.app"),
                cache.parse_count(),
            ),
            (1, 2, 1, 2, true, 2, 3, true, false, 3),
        );
    }

    fn node_count(deps_json: Result<String, CommandError>) -> usize {
        let document: serde_json::Value =
            serde_json::from_str(&deps_json.expect("deps json")).expect("valid json");
        document["nodes"].as_array().expect("nodes array").len()
    }

    #[test]
    fn cached_answers_equal_the_in_process_answers_across_formats_and_options() {
        let fixture = Fixture::new();
        let file = fixture.write(
            "core/Order.kt",
            "package shop.order\n\n/** A placed order. */\ndata class Order(private val id: Int)\n",
        );
        let cache = SkeletonCache::default();
        let rich = RenderOptions::default().with_private().with_doc();

        let outline_md = pair(
            cache.outline(&fixture.root, &file, Format::Md, &rich),
            crate::outline(&fixture.root, &file, Format::Md, &rich),
        );
        let outline_json = pair(
            cache.outline(&fixture.root, &file, Format::Json, &rich),
            crate::outline(&fixture.root, &file, Format::Json, &rich),
        );
        let deps_md = pair(
            cache.deps(&fixture.root, DepLevel::Package, Format::Md),
            crate::deps(&fixture.root, DepLevel::Package, Format::Md),
        );
        let deps_json = pair(
            cache.deps(&fixture.root, DepLevel::File, Format::Json),
            crate::deps(&fixture.root, DepLevel::File, Format::Json),
        );
        let deps_dot = pair(
            cache.deps(&fixture.root, DepLevel::Package, Format::Dot),
            crate::deps(&fixture.root, DepLevel::Package, Format::Dot),
        );

        assert_eq!(
            (outline_md, outline_json, deps_md, deps_json, deps_dot),
            (true, true, true, true, true),
        );
    }

    fn pair(
        cached: Result<String, CommandError>,
        in_process: Result<String, CommandError>,
    ) -> bool {
        cached.expect("cached answer") == in_process.expect("in-process answer")
    }

    /// A file with a localized parse error is recovered and marked partial, cached like any other,
    /// and its cached outline equals the in-process one; editing it re-parses rather than serving
    /// the stale partial. A leading `;` in the class body is the KT-52 semicolon gap.
    #[test]
    fn a_partial_file_is_cached_matches_in_process_and_re_parses_on_edit() {
        let fixture = Fixture::new();
        let partial = "package shop.order\n\nclass Ledger {;\n    fun balance(): Int = 0\n}\n";
        let file = fixture.write("Ledger.kt", partial);
        let cache = SkeletonCache::default();
        let options = RenderOptions::default();

        let first = cache.outline(&fixture.root, &file, Format::Md, &options);
        let repeat = cache.outline(&fixture.root, &file, Format::Md, &options);
        let parses_after_repeat = cache.parse_count();
        let in_process =
            crate::outline(&fixture.root, &file, Format::Md, &options).expect("in-process outline");

        fixture.write(
            "Ledger.kt",
            "package shop.order\n\nclass Ledger {\n    fun paid(): Int = 0\n}\n",
        );
        let after_edit = cache
            .outline(&fixture.root, &file, Format::Md, &options)
            .expect("edited outline");

        let first = first.expect("first outline");
        assert_eq!(
            (
                first == repeat.expect("repeat outline"),
                parses_after_repeat,
                first.contains("// partial:"),
                first.contains("fun balance"),
                first == in_process,
                after_edit.contains("// partial:"),
                after_edit.contains("fun paid"),
                cache.parse_count(),
            ),
            (true, 1, true, true, true, false, true, 2),
        );
    }
}
