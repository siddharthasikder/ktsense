//! KT-33 end to end: the server honours `roots/list`, and picks the advertised root that holds the
//! file a call names.
//!
//! The unit test in `src/roots.rs` pins the resolution rule; this one proves the wiring, which is
//! the part that can silently not happen: the server has to ask the client, wait for the answer
//! before dispatching, and then pass the chosen root down to the command. Every assertion reads the
//! root out of the answer's own `structuredContent`, because that is the root the command was
//! actually run against rather than the one we hoped for.

mod session;

use std::path::{Path, PathBuf};

use serde_json::json;
use session::{structured, Session};

/// Two workspaces and a third directory that is neither, so "it picked the root holding the file"
/// cannot be satisfied by accident.
struct Workspaces {
    _temp: tempfile::TempDir,
    alpha: PathBuf,
    beta: PathBuf,
    elsewhere: PathBuf,
}

const ALPHA_FILE: &str = "core/src/main/kotlin/alpha/Alpha.kt";
const BETA_FILE: &str = "app/src/main/kotlin/beta/Beta.kt";

impl Workspaces {
    fn build() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let alpha = temp.path().join("alpha");
        let beta = temp.path().join("beta");
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("tree");
        for (root, relative, package) in [(&alpha, ALPHA_FILE, "alpha"), (&beta, BETA_FILE, "beta")]
        {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("tree");
            std::fs::write(
                &path,
                format!("package {package}\n\ninterface Marker {{\n    fun mark(): String\n}}\n"),
            )
            .expect("write");
        }
        Self {
            _temp: temp,
            alpha,
            beta,
            elsewhere,
        }
    }

    fn resolved(&self, root: &Path) -> String {
        std::fs::canonicalize(root)
            .expect("canonical")
            .display()
            .to_string()
    }
}

#[test]
fn with_two_advertised_roots_each_call_is_answered_about_the_one_holding_its_file() {
    let tree = Workspaces::build();
    let mut server = Session::rooted(tree.elsewhere.as_path(), ".")
        .advertising(&[tree.alpha.as_path(), tree.beta.as_path()]);
    server.initialize();

    let in_alpha = server.call(2, "get_kotlin_outline", json!({ "file": ALPHA_FILE }));
    let in_beta = server.call(3, "get_kotlin_outline", json!({ "file": BETA_FILE }));
    let named_by_the_call = server.call(
        4,
        "get_kotlin_outline",
        json!({ "file": ALPHA_FILE, "root": tree.alpha.display().to_string() }),
    );
    let asked = server.roots_requests();
    let (exit, stderr) = server.finish();

    let observed = (
        structured(&in_alpha)["root"].clone(),
        structured(&in_alpha)["files"].clone(),
        structured(&in_beta)["root"].clone(),
        structured(&named_by_the_call)["root"].clone(),
        asked,
        exit,
        stderr.is_empty(),
    );
    assert_eq!(
        observed,
        (
            json!(tree.alpha.display().to_string()),
            json!([ALPHA_FILE]),
            json!(tree.beta.display().to_string()),
            json!(tree.alpha.display().to_string()),
            1,
            Some(0),
            true,
        ),
        "the server should ask for roots once and then answer about the root holding each file"
    );
}

#[test]
fn a_client_that_advertises_no_roots_is_never_asked_and_keeps_the_configured_root() {
    let tree = Workspaces::build();
    let mut server = Session::rooted(tree.alpha.as_path(), ".");
    server.initialize();

    let answered = server.call(2, "get_kotlin_outline", json!({ "file": ALPHA_FILE }));
    let asked = server.roots_requests();
    let (exit, stderr) = server.finish();

    let observed = (
        structured(&answered)["root"].clone(),
        structured(&answered)["files"].clone(),
        asked,
        exit,
        stderr.is_empty(),
    );
    assert_eq!(
        observed,
        (
            json!(tree.resolved(&tree.alpha)),
            json!([ALPHA_FILE]),
            0,
            Some(0),
            true,
        ),
        "with no roots capability the server must not ask, and must keep its --root"
    );
}
