//! Pure test of import-graph resolution, built from hand-made skeletons that mirror the
//! `multi-module` fixture. Proves the resolution and cycle rules without a filesystem; the CLI
//! test proves the same numbers end to end against the real fixture.

use ktsense_core::{build_import_graph, Declaration, DepLevel, FileSkeleton};

fn file(path: &str, package: &str, imports: &[&str], declared: &str) -> FileSkeleton {
    FileSkeleton::new(path)
        .in_package(package)
        .with_imports(imports.iter().map(|import| import.to_string()).collect())
        .with_declarations(vec![Declaration::class(declared, 1)])
}

fn workspace() -> Vec<FileSkeleton> {
    vec![
        file("core/OrderId.kt", "shop.order", &[], "OrderId"),
        file(
            "core/OrderRepository.kt",
            "shop.order",
            &[],
            "OrderRepository",
        ),
        file("core/Order.kt", "shop.order", &[], "Order"),
        file(
            "db/JdbcOrderRepository.kt",
            "shop.db",
            &["shop.order.Order", "shop.order.OrderRepository"],
            "JdbcOrderRepository",
        ),
        file(
            "db/InMemoryOrderRepository.kt",
            "shop.db",
            &[
                "java.util.concurrent.ConcurrentHashMap",
                "java.util.concurrent.atomic.AtomicLong",
                "shop.order.Order",
                "shop.order.OrderId",
            ],
            "InMemoryOrderRepository",
        ),
        file(
            "app/CheckoutService.kt",
            "shop.app.checkout",
            &["shop.app.reporting.AuditTrail", "shop.order.Order"],
            "CheckoutService",
        ),
        file(
            "app/OrderImporter.kt",
            "shop.app.checkout",
            &["shop.db.InMemoryOrderRepository", "shop.order.Order"],
            "OrderImporter",
        ),
        file(
            "app/CheckoutConfig.kt",
            "shop.app.checkout",
            &[],
            "CheckoutConfig",
        ),
        file("app/AuditTrail.kt", "shop.app.reporting", &[], "AuditTrail"),
        file(
            "app/ReportBackfill.kt",
            "shop.app.reporting",
            &[
                "java.time.Instant",
                "shop.app.checkout.CheckoutConfig",
                "shop.order.Order",
            ],
            "ReportBackfill",
        ),
    ]
}

#[test]
fn the_package_graph_matches_the_fixture_contract_exactly() {
    let graph = build_import_graph(&workspace(), DepLevel::Package);

    let edges: Vec<(&str, &str)> = graph
        .edges
        .iter()
        .map(|edge| (edge.from.as_str(), edge.to.as_str()))
        .collect();
    let external: Vec<(&str, &str)> = graph
        .external
        .iter()
        .map(|external| (external.source.as_str(), external.import.as_str()))
        .collect();

    let observed = (graph.nodes, edges, external, graph.cycles);

    assert_eq!(
        observed,
        (
            vec![
                "shop.app.checkout".to_string(),
                "shop.app.reporting".to_string(),
                "shop.db".to_string(),
                "shop.order".to_string(),
            ],
            vec![
                ("shop.app.checkout", "shop.app.reporting"),
                ("shop.app.checkout", "shop.db"),
                ("shop.app.checkout", "shop.order"),
                ("shop.app.reporting", "shop.app.checkout"),
                ("shop.app.reporting", "shop.order"),
                ("shop.db", "shop.order"),
            ],
            vec![
                ("shop.app.reporting", "java.time.Instant"),
                ("shop.db", "java.util.concurrent.ConcurrentHashMap"),
                ("shop.db", "java.util.concurrent.atomic.AtomicLong"),
            ],
            vec![vec![
                "shop.app.checkout".to_string(),
                "shop.app.reporting".to_string(),
            ]],
        )
    );
}

#[test]
fn the_file_graph_resolves_to_files_and_is_acyclic_where_the_package_graph_cycles() {
    let graph = build_import_graph(&workspace(), DepLevel::File);

    let closes_the_package_cycle = graph
        .edges
        .iter()
        .any(|edge| edge.from == "app/CheckoutService.kt" && edge.to == "app/AuditTrail.kt");

    let observed = (graph.nodes.len(), closes_the_package_cycle, graph.cycles);

    assert_eq!(observed, (10, true, Vec::<Vec<String>>::new()));
}
