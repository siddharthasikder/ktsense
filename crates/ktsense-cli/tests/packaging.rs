//! The release layout is only correct if an extracted archive can find its own engine.
//!
//! `scripts/package-release.sh` stages `bin/ktsense` beside `libexec/kmp-lsp` precisely so that
//! discovery's `<exe-dir>/../libexec/kmp-lsp` rule resolves the bundled engine with no
//! `KTSENSE_LSP_PATH` set. This test packages a real Linux tarball with the fake engine standing in
//! for `kmp-lsp`, extracts it, and runs the packaged binary against an empty root: if the tar
//! listing or the layout drifts, the engine is reported unavailable and the single assertion fails.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const TARGET: &str = "x86_64-unknown-linux-musl";
const VERSION: &str = "0.0.0-test";

fn fake_lsp() -> PathBuf {
    let path = Path::new(env!("CARGO_BIN_EXE_ktsense")).with_file_name("fake_lsp");
    if !path.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let built = Command::new(cargo)
            .args(["build", "-p", "ktsense-lsp", "--bin", "fake_lsp"])
            .current_dir(WORKSPACE_ROOT)
            .status()
            .expect("cargo runs");
        assert!(built.success(), "building fake_lsp failed");
    }
    path
}

fn package(dest: &Path, engine_dir: &Path) {
    let root = Path::new(WORKSPACE_ROOT);
    let script = root.join("scripts/package-release.sh");
    let status = Command::new("bash")
        .arg(&script)
        .args(["--target", TARGET, "--version", VERSION])
        .arg("--bin")
        .arg(env!("CARGO_BIN_EXE_ktsense"))
        .arg("--engine")
        .arg(engine_dir)
        .arg("--skill")
        .arg(root.join("contrib/agent-skill/SKILL.md"))
        .arg("--license")
        .arg(root.join("LICENSE"))
        .arg("--license-upstream")
        .arg(root.join("LICENSE.kmp-lsp"))
        .arg("--dest")
        .arg(dest)
        .status()
        .expect("bash runs");
    assert!(status.success(), "package-release.sh failed");
}

fn tar_entries(tarball: &Path) -> Vec<String> {
    let out = Command::new("tar")
        .arg("tzf")
        .arg(tarball)
        .output()
        .expect("tar runs");
    assert!(out.status.success(), "tar tzf failed");
    let mut entries: Vec<String> = String::from_utf8(out.stdout)
        .expect("utf-8")
        .lines()
        .map(|line| line.trim_end_matches('/').to_string())
        .filter(|line| !line.is_empty())
        .collect();
    entries.sort();
    entries
}

#[test]
fn a_packaged_archive_finds_its_bundled_engine_after_extraction() {
    let temp = tempfile::tempdir().expect("temp dir");
    let engine_dir = temp.path().join("engine");
    std::fs::create_dir_all(&engine_dir).expect("engine dir");
    std::fs::copy(fake_lsp(), engine_dir.join("kmp-lsp")).expect("stage fake engine");

    let dest = temp.path().join("dist");
    package(&dest, &engine_dir);

    let name = format!("ktsense-{VERSION}-{TARGET}");
    let tarball = dest.join(format!("{name}.tar.gz"));
    let extract = temp.path().join("extract");
    std::fs::create_dir_all(&extract).expect("extract dir");
    let untar = Command::new("tar")
        .arg("xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&extract)
        .status()
        .expect("tar runs");
    assert!(untar.success(), "extraction failed");

    let root = temp.path().join("root");
    std::fs::create_dir_all(&root).expect("root dir");
    let run = Command::new(extract.join(&name).join("bin/ktsense"))
        .args(["--root"])
        .arg(&root)
        .args(["--format", "json", "status"])
        .env("XDG_RUNTIME_DIR", temp.path())
        .env_remove("KTSENSE_LSP_PATH")
        .output()
        .expect("packaged binary runs");
    let report: serde_json::Value =
        serde_json::from_slice(&run.stdout).expect("json status output");
    let engine = &report["engine"];
    let binary = engine["binary"].as_str().unwrap_or_default();

    let manifest = std::fs::read_to_string(dest.join(format!("{name}.tar.gz.sha256")))
        .expect("sha256 manifest exists");

    assert_eq!(
        (
            run.status.code(),
            tar_entries(&tarball),
            engine["version"].clone(),
            engine["compatibility"].clone(),
            binary.contains("libexec/kmp-lsp"),
            manifest.contains(&format!("{name}.tar.gz")),
        ),
        (
            Some(0),
            vec![
                name.clone(),
                format!("{name}/LICENSE"),
                format!("{name}/LICENSE.kmp-lsp"),
                format!("{name}/SKILL.md"),
                format!("{name}/bin"),
                format!("{name}/bin/ktsense"),
                format!("{name}/libexec"),
                format!("{name}/libexec/kmp-lsp"),
            ],
            serde_json::json!("0.26.0"),
            serde_json::json!("supported"),
            true,
            true,
        ),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );
}
