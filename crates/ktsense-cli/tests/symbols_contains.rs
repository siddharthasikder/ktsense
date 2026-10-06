//! `symbols --contains` over the `multi-module` fixture.
//!
//! The partial-name search reads the workspace's own syntax skeletons, so it needs no engine and
//! runs in the default install-free suite. The binary runs with the workspace root as its working
//! directory and a relative `--root`, so the answer carries fixture-relative paths.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn symbols(args: &[&str]) -> Run {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(WORKSPACE_ROOT)
        .args(args)
        .output()
        .expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

/// A `--contains Repository` search names every repository type, most relevant first, states its
/// source, and exits successfully. The whole composed result is one assertion: exit, the qualified
/// names in rank order, the source header, and the silent stderr.
#[test]
fn contains_lists_every_repository_in_rank_order_from_the_syntax_index() {
    let run = symbols(&[
        "symbols",
        "--contains",
        "Repository",
        "--root",
        "fixtures/multi-module",
    ]);

    let qualified_names: Vec<String> = run
        .stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|token| token.contains("Repository"))
        .map(str::to_string)
        .collect();
    let states_source = run.stdout.contains("source: syntax index");

    assert_eq!(
        (run.code, qualified_names, states_source, run.stderr),
        (
            Some(0),
            vec![
                "shop.order.OrderRepository".to_string(),
                "shop.db.JdbcOrderRepository".to_string(),
                "shop.db.InMemoryOrderRepository".to_string(),
            ],
            true,
            String::new(),
        )
    );
}

/// An extension function is listed under its own simple name with a receiver-free qualified name:
/// `--contains blankToNull` reports `shop.order.blankToNull`, not `shop.order.String?.blankToNull`,
/// while the receiver survives in the signature column (KT-121).
#[test]
fn contains_names_an_extension_by_its_simple_name_with_a_receiver_free_qualified_name() {
    let run = symbols(&[
        "symbols",
        "--contains",
        "blankToNull",
        "--root",
        "fixtures/multi-module",
    ]);

    let row = run
        .stdout
        .lines()
        .find(|line| line.contains("Order.kt:"))
        .map(str::to_string);

    assert_eq!(
        (run.code, row, run.stderr),
        (
            Some(0),
            Some(
                "shop.order.blankToNull  fun  core/src/main/kotlin/shop/order/Order.kt:11  internal fun String?.blankToNull(): String?"
                    .to_string()
            ),
            String::new(),
        )
    );
}

/// Primary-constructor `val`/`var` properties are listed under their class by partial name even
/// though the model carries them as parameters rather than children, so `--contains customerEmail`
/// finds `shop.order.Order.customerEmail`. A class-body property was already indexed and is listed
/// the same way, so `--contains currency` finds `shop.app.checkout.CheckoutConfig.currency` (KT-123).
#[test]
fn contains_finds_constructor_and_class_body_properties_under_their_class() {
    let constructor_property = symbols(&[
        "symbols",
        "--contains",
        "customerEmail",
        "--root",
        "fixtures/multi-module",
    ]);
    let class_body_property = symbols(&[
        "symbols",
        "--contains",
        "currency",
        "--root",
        "fixtures/multi-module",
    ]);
    let row = |run: &Run| {
        run.stdout
            .lines()
            .find(|line| line.contains(".kt:"))
            .map(str::to_string)
    };

    assert_eq!(
        (
            constructor_property.code,
            row(&constructor_property),
            class_body_property.code,
            row(&class_body_property),
        ),
        (
            Some(0),
            Some(
                "shop.order.Order.customerEmail  val  core/src/main/kotlin/shop/order/Order.kt:5  val customerEmail: String"
                    .to_string()
            ),
            Some(0),
            Some(
                "shop.app.checkout.CheckoutConfig.currency  val  app/src/main/kotlin/shop/app/checkout/CheckoutConfig.kt:4  const val currency: String"
                    .to_string()
            ),
        )
    );
}

/// A query nothing contains is a real "no such declaration" answer: exit 1, and the KT-87 scope
/// wording saying only this workspace was searched.
#[test]
fn contains_that_matches_nothing_exits_one_with_the_workspace_scope_wording() {
    let run = symbols(&[
        "symbols",
        "--contains",
        "Nonexistent",
        "--root",
        "fixtures/multi-module",
    ]);

    assert_eq!(
        (
            run.code,
            run.stderr.contains("in this workspace"),
            run.stdout,
        ),
        (Some(1), true, String::new())
    );
}

/// The JSON answer carries what the Markdown header says: where the matches came from and how many
/// the cap left out, so a tool chaining on JSON learns the listing is partial. `--limit 1` over the
/// three repository types keeps the best-ranked one and reports the other two.
#[test]
fn contains_json_states_its_source_and_the_matches_the_cap_hid() {
    let run = symbols(&[
        "symbols",
        "--contains",
        "Repository",
        "--limit",
        "1",
        "--format",
        "json",
        "--root",
        "fixtures/multi-module",
    ]);
    let parsed: serde_json::Value = serde_json::from_str(&run.stdout).expect("json stdout");

    assert_eq!(
        (
            run.code,
            parsed["source"].clone(),
            parsed["matches"].as_array().map(Vec::len),
            parsed["matches"][0]["fqn"].clone(),
            parsed["more_matches"].clone(),
        ),
        (
            Some(0),
            serde_json::json!("syntax index"),
            Some(1),
            serde_json::json!("shop.order.OrderRepository"),
            serde_json::json!(2),
        )
    );
}
