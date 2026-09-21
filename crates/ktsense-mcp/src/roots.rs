//! Which workspace root a tool call is about.
//!
//! Three sources, in this order:
//!
//! 1. The call's own `root` argument. The caller was specific, so nothing overrides it.
//! 2. The roots the client advertised through `roots/list`: the first one that contains the file or
//!    directory the call names, else the first one it listed. An editor with two projects open sends
//!    both, and answering about the wrong one is worse than answering slowly, because a widened or
//!    shifted root does not fail, it answers about the wrong code (see AGENTS.md).
//! 3. Neither, expressed as `None`: the runner then uses the root the server was started with,
//!    which is `--root` and, failing that, the working directory. That fallback already lives in the
//!    CLI, so it is not restated here.
//!
//! Containment is decided against the filesystem, because it has to be: an absolute subject is
//! compared by prefix, but a relative one is only under a root if it is actually there. The card's
//! acceptance is a file that exists in one of two roots, so a test that faked existence would not
//! be testing the thing that matters.

use std::path::{Path, PathBuf};

/// The roots a client advertised, in the order it listed them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClientRoots(Vec<PathBuf>);

impl ClientRoots {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self(roots)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The root to answer about, or `None` to leave the choice to the server's configured root.
    ///
    /// `subject` is the file or directory the call named, when it named one; a call about the whole
    /// workspace has none and takes the first advertised root.
    pub fn resolve(&self, call: Option<String>, subject: Option<&str>) -> Option<String> {
        if let Some(named) = call {
            return Some(named);
        }
        let containing = subject.and_then(|subject| self.containing(subject));
        containing
            .or_else(|| self.0.first().cloned())
            .map(|root| root.display().to_string())
    }

    fn containing(&self, subject: &str) -> Option<PathBuf> {
        let subject = Path::new(subject);
        self.0.iter().find(|root| holds(root, subject)).cloned()
    }
}

/// Whether `root` holds `subject`. An absolute subject is judged by prefix, and by the resolved
/// prefix when either side carries a symlink or a `..` that a textual compare would miss. A
/// relative subject is judged by whether it exists under the root, which is the only honest test:
/// the same relative path is valid under several roots at once.
fn holds(root: &Path, subject: &Path) -> bool {
    if !subject.is_absolute() {
        return root.join(subject).exists();
    }
    if subject.starts_with(root) {
        return true;
    }
    match (std::fs::canonicalize(root), std::fs::canonicalize(subject)) {
        (Ok(root), Ok(subject)) => subject.starts_with(root),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two workspaces, each holding a file of the same relative name, plus a file in neither.
    struct TwoRoots {
        _temp: tempfile::TempDir,
        first: PathBuf,
        second: PathBuf,
        elsewhere: PathBuf,
    }

    impl TwoRoots {
        fn build() -> Self {
            let temp = tempfile::tempdir().expect("temp dir");
            let first = temp.path().join("alpha");
            let second = temp.path().join("beta");
            for (root, relative) in [
                (&first, "core/src/main/kotlin/Alpha.kt"),
                (&second, "app/src/main/kotlin/Beta.kt"),
            ] {
                let path = root.join(relative);
                std::fs::create_dir_all(path.parent().expect("parent")).expect("tree");
                std::fs::write(&path, "package p\n").expect("write");
            }
            let elsewhere = temp.path().join("gamma/Outside.kt");
            std::fs::create_dir_all(elsewhere.parent().expect("parent")).expect("tree");
            std::fs::write(&elsewhere, "package p\n").expect("write");
            Self {
                _temp: temp,
                first,
                second,
                elsewhere,
            }
        }

        fn roots(&self) -> ClientRoots {
            ClientRoots::new(vec![self.first.clone(), self.second.clone()])
        }
    }

    #[test]
    fn with_two_roots_the_one_holding_the_requested_file_wins_however_the_file_was_named() {
        let tree = TwoRoots::build();
        let roots = tree.roots();
        let first = tree.first.display().to_string();
        let second = tree.second.display().to_string();

        let observed = (
            roots.resolve(None, Some("core/src/main/kotlin/Alpha.kt")),
            roots.resolve(None, Some("app/src/main/kotlin/Beta.kt")),
            roots.resolve(
                None,
                Some(
                    &tree
                        .second
                        .join("app/src/main/kotlin/Beta.kt")
                        .display()
                        .to_string(),
                ),
            ),
            roots.resolve(None, Some(&tree.elsewhere.display().to_string())),
            roots.resolve(None, None),
            roots.resolve(
                Some("/named/by/the/call".to_string()),
                Some("core/src/main/kotlin/Alpha.kt"),
            ),
            ClientRoots::default().resolve(None, Some("core/src/main/kotlin/Alpha.kt")),
        );

        assert_eq!(
            observed,
            (
                Some(first.clone()),
                Some(second.clone()),
                Some(second),
                Some(first.clone()),
                Some(first),
                Some("/named/by/the/call".to_string()),
                None,
            ),
            "a subject in neither root, and a workspace-wide call, both take the first root; \
             no advertised roots leaves the choice to the configured one"
        );
    }
}
