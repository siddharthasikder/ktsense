//! KT-130: an exact `symbols` lookup leaves out engine declarations inside directories the
//! workspace walks skip, such as an IDE's `bin/` copy of the sources, and says how many it left out.
//!
//! The fake engine's command-mode `find` stands in for an engine run in a checkout that does not
//! gitignore `bin/`: it reports the interface method twice, once from `core/src` and once from the
//! IDE copy under `core/bin`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::json;

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const FIXTURE: &str = "fixtures/multi-module";
const REPOSITORY: &str = "core/src/main/kotlin/shop/order/OrderRepository.kt";
const REPOSITORY_IDE_COPY: &str = "core/bin/main/shop/order/OrderRepository.kt";

fn fake_lsp() -> PathBuf {
    static FAKE: OnceLock<PathBuf> = OnceLock::new();
    FAKE.get_or_init(|| {
        let path = Path::new(env!("CARGO_BIN_EXE_ktsense")).with_file_name("fake_lsp");
        if !path.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
            let status = Command::new(cargo)
                .args(["build", "-p", "ktsense-lsp", "--bin", "fake_lsp"])
                .current_dir(WORKSPACE_ROOT)
                .status()
                .expect("cargo runs");
            assert!(status.success(), "building fake_lsp failed");
        }
        path
    })
    .clone()
}

fn absolute(relative: &str) -> String {
    Path::new(WORKSPACE_ROOT)
        .join(FIXTURE)
        .canonicalize()
        .expect("fixture exists")
        .join(relative)
        .display()
        .to_string()
}

#[test]
fn symbols_leaves_out_declarations_inside_ignored_directories_and_counts_them() {
    let find = json!([
        { "file": absolute(REPOSITORY_IDE_COPY), "line": 3, "col": 11, "name": "OrderRepository" },
        { "file": absolute(REPOSITORY), "line": 3, "col": 11, "name": "OrderRepository" },
    ]);
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(WORKSPACE_ROOT)
        .env("KTSENSE_LSP_PATH", fake_lsp())
        .env("KTSENSE_NO_AUTOSTART", "1")
        .env("FAKE_CMD_STDOUT", find.to_string())
        .args(["--root", FIXTURE, "symbols", "OrderRepository"])
        .output()
        .expect("binary runs");

    insta::assert_snapshot!(format!(
        "exit {}\nstderr {}\n---\n{}",
        output.status.code().expect("exited normally"),
        String::from_utf8_lossy(&output.stderr).trim(),
        String::from_utf8_lossy(&output.stdout)
    ));
}
