//! Golden snapshots of `outline` for every Kotlin fixture file, in both Markdown and JSON. One
//! parameterized harness drives every file of every fixture and both formats, so a new fixture
//! file or a new format needs a single edit to the tables here, not a hand-written test each.
//!
//! Both fixtures are covered: `tiny-app` (fifteen `.kt` sources and two `.kts` Gradle scripts) and
//! `multi-module` (nine `.kt` sources and five `.kts` scripts). Every file the CLI treats as Kotlin
//! is listed; the Java sources and the generated `bin/`, `.gradle` and IDE-metadata trees are
//! deliberately omitted, since those are not `outline` inputs worth pinning.
//!
//! The binary is run with its working directory set to the fixture root and given a workspace
//! relative path, so the path echoed in the output is that relative argument rather than this
//! host's absolute checkout location. The snapshots are therefore identical on any machine.
//!
//! Each snapshot is one structured record of exit code, standard error and standard output, so a
//! regression in the exit status or a stray diagnostic on stderr fails the snapshot just as a
//! change in rendered text would.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

struct Fixture {
    name: &'static str,
    root: &'static str,
    files: &'static [&'static str],
}

/// Fixture files sorted for a stable iteration order. Each list is the exact set of `.kt` and
/// `.kts` files under the fixture's source tree; generated `bin/`, `.gradle` and IDE trees and the
/// Java sources are excluded.
const FIXTURES: [Fixture; 2] = [
    Fixture {
        name: "tiny-app",
        root: concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/tiny-app"),
        files: &[
            "build.gradle.kts",
            "settings.gradle.kts",
            "src/main/kotlin/app/Main.kt",
            "src/main/kotlin/app/domain/Outcome.kt",
            "src/main/kotlin/app/domain/Repository.kt",
            "src/main/kotlin/app/domain/Role.kt",
            "src/main/kotlin/app/domain/User.kt",
            "src/main/kotlin/app/domain/annotations.kt",
            "src/main/kotlin/app/service/InMemoryUserRepository.kt",
            "src/main/kotlin/app/service/Service.kt",
            "src/main/kotlin/app/service/ServiceRegistry.kt",
            "src/main/kotlin/app/service/UserService.kt",
            "src/main/kotlin/app/service/Validator.kt",
            "src/main/kotlin/app/util/Clock.kt",
            "src/main/kotlin/app/util/Page.kt",
            "src/main/kotlin/app/util/Retry.kt",
            "src/main/kotlin/app/util/internals.kt",
        ],
    },
    Fixture {
        name: "multi-module",
        root: concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/multi-module"),
        files: &[
            "app/build.gradle.kts",
            "app/src/main/kotlin/shop/app/checkout/CheckoutConfig.kt",
            "app/src/main/kotlin/shop/app/checkout/CheckoutService.kt",
            "app/src/main/kotlin/shop/app/checkout/OrderImporter.kt",
            "app/src/main/kotlin/shop/app/reporting/AuditTrail.kt",
            "app/src/main/kotlin/shop/app/reporting/ReportBackfill.kt",
            "build.gradle.kts",
            "core/build.gradle.kts",
            "core/src/main/kotlin/shop/order/Order.kt",
            "core/src/main/kotlin/shop/order/OrderRepository.kt",
            "db/build.gradle.kts",
            "db/src/main/kotlin/shop/db/InMemoryOrderRepository.kt",
            "db/src/main/kotlin/shop/db/JdbcOrderRepository.kt",
            "settings.gradle.kts",
        ],
    },
];

const FORMATS: [&str; 2] = ["md", "json"];

fn outline(fixture_root: &str, relative_path: &str, format: &str) -> String {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(fixture_root)
        .args(["outline", relative_path, "--format", format])
        .output()
        .expect("binary runs");

    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let exit = output
        .status
        .code()
        .map_or_else(|| "signal".to_string(), |code| code.to_string());
    let stderr_line = if stderr.is_empty() {
        "(empty)".to_string()
    } else {
        stderr
    };

    format!("exit: {exit}\nstderr: {stderr_line}\n--- stdout ---\n{stdout}")
}

fn snapshot_name(fixture_name: &str, relative_path: &str, format: &str) -> String {
    let sanitize = |value: &str| -> String {
        value
            .chars()
            .map(|character| {
                if character.is_alphanumeric() {
                    character
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!(
        "{format}__{}__{}",
        sanitize(fixture_name),
        sanitize(relative_path)
    )
}

#[test]
fn outline_of_every_kotlin_fixture_is_pinned_in_both_formats() {
    for fixture in &FIXTURES {
        for relative_path in fixture.files {
            for format in FORMATS {
                insta::assert_snapshot!(
                    snapshot_name(fixture.name, relative_path, format),
                    outline(fixture.root, relative_path, format)
                );
            }
        }
    }
}
