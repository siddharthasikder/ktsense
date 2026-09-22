//! Engine discovery through a real Homebrew-style install layout (KT-62).
//!
//! Each case builds the layout `Formula/ktsense.rb` produces: the real `ktsense` in a Cellar-style
//! directory with its bundled `libexec/kmp-lsp` beside it, and a prefix-level `bin/ktsense` symlink
//! that has no engine of its own. Discovery is then driven through the symlink, which is what a user
//! on `PATH` invokes and what `current_exe` reports on macOS. A string that merely looks like a brew
//! path cannot tell the two apart, so the layout is real files and a real link, and the assertions
//! pin the candidate ktsense selects rather than what `canonicalize` returns.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

use ktsense_lsp::{locate_binary_from, LSP_BINARY};

const CELLAR_BIN: &str = "Cellar/ktsense/0.1.0/bin";
const CELLAR_LIBEXEC: &str = "Cellar/ktsense/0.1.0/libexec";
const PREFIX_BIN: &str = "prefix/bin";
const EXECUTABLE: &str = "ktsense";

/// An install to run discovery against: `link` is the prefix symlink a user invokes, `absent_engine`
/// and `override_engine` are the two override values the lookup order has to tell apart, and
/// `absent_executable` shares the real binary's directory without existing, so resolving it fails
/// while the engine beside it is still there to be found.
struct Install {
    root: PathBuf,
    canonical_root: PathBuf,
    link: PathBuf,
    override_engine: PathBuf,
    absent_engine: PathBuf,
    absent_executable: PathBuf,
}

impl Install {
    /// A complete install: the engine sits beside the real executable, and nowhere else.
    fn with_bundled_engine(name: &str) -> Self {
        let install = Self::staged(name);
        write_file(&install.root.join(CELLAR_LIBEXEC).join(LSP_BINARY));
        install
    }

    /// The same layout with no engine anywhere, so the only remaining tier is `PATH`.
    fn without_bundled_engine(name: &str) -> Self {
        Self::staged(name)
    }

    fn staged(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "ktsense-kt62-{name}-{pid}",
            pid = std::process::id()
        ));
        fs::remove_dir_all(&root).ok();
        for relative in [CELLAR_BIN, CELLAR_LIBEXEC, PREFIX_BIN, "override"] {
            fs::create_dir_all(root.join(relative)).expect("layout directory");
        }

        let real_executable = root.join(CELLAR_BIN).join(EXECUTABLE);
        write_executable(&real_executable);
        let link = root.join(PREFIX_BIN).join(EXECUTABLE);
        symlink(Path::new("../..").join(CELLAR_BIN).join(EXECUTABLE), &link)
            .expect("prefix symlink");

        let override_engine = root.join("override").join(LSP_BINARY);
        write_executable(&override_engine);

        Self {
            canonical_root: fs::canonicalize(&root).expect("canonical root"),
            link,
            override_engine,
            absent_engine: root.join("override").join("kmp-lsp-that-was-uninstalled"),
            absent_executable: root.join(CELLAR_BIN).join("ktsense-that-was-replaced"),
            root,
        }
    }

    /// What discovery selects when launched through the prefix symlink.
    fn selected(&self, env_override: Option<PathBuf>) -> String {
        self.selected_for(env_override, &self.link)
    }

    /// What discovery selects for a given executable path, spelled relative to this install so the
    /// expectation reads as the layout and not as the platform's temporary directory. A selection
    /// that names no file, which is the bare `PATH` fallback, is reported verbatim.
    fn selected_for(&self, env_override: Option<PathBuf>, exe: &Path) -> String {
        let chosen = locate_binary_from(env_override, Some(exe.to_path_buf()));
        match fs::canonicalize(&chosen) {
            Ok(real) => real
                .strip_prefix(&self.canonical_root)
                .unwrap_or(&real)
                .display()
                .to_string(),
            Err(_) => chosen.display().to_string(),
        }
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).ok();
    }
}

fn write_file(path: &Path) {
    fs::write(path, b"KT-62 test stand-in, never executed\n").expect("layout file");
}

fn write_executable(path: &Path) {
    write_file(path);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("executable mode");
}

#[test]
fn an_engine_beside_the_real_binary_is_found_through_the_prefix_symlink() {
    let install = Install::with_bundled_engine("bundled");

    assert_eq!(
        install.selected(None),
        format!("{CELLAR_LIBEXEC}/{LSP_BINARY}"),
        "discovery derived the libexec candidate from the symlink rather than the real binary"
    );
}

#[test]
fn the_override_leads_and_path_stays_last_in_a_symlinked_install() {
    let bundled = Install::with_bundled_engine("tiers-bundled");
    let bare = Install::without_bundled_engine("tiers-bare");

    let observed = (
        bundled.selected(Some(bundled.override_engine.clone())),
        bundled.selected(Some(bundled.absent_engine.clone())),
        bare.selected(Some(bare.override_engine.clone())),
        bare.selected(None),
    );

    assert_eq!(
        observed,
        (
            format!("override/{LSP_BINARY}"),
            format!("{CELLAR_LIBEXEC}/{LSP_BINARY}"),
            format!("override/{LSP_BINARY}"),
            LSP_BINARY.to_string(),
        )
    );
}

#[test]
fn an_executable_that_cannot_be_resolved_keeps_its_libexec_candidate() {
    let install = Install::with_bundled_engine("unresolvable");

    assert_eq!(
        install.selected_for(None, &install.absent_executable),
        format!("{CELLAR_LIBEXEC}/{LSP_BINARY}"),
        "a failed resolution dropped the executable-derived tier instead of keeping it uncanonicalized"
    );
}
