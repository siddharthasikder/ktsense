//! Locating a declaration that lives inside a function or property body, which the file skeleton
//! drops with the body. The `symbols` enrichment falls back to this so a function-local class,
//! object or function is qualified through its enclosing declarations rather than reported as a
//! bare name (KT-98).

use ktsense_core::DeclKind;
use ktsense_syntax::locate_local;

const NESTED_IN_A_LAMBDA: &str = r#"
package io.ktor.server.application

class ApplicationPluginTest {
    fun test_routing_scoped_install() = testApplication {
        class Config(var data: String)
    }
}
"#;

/// A class declared inside a function body, itself inside a lambda argument, is found with its real
/// kind and qualified through the package and every enclosing declaration.
#[test]
fn a_class_in_a_function_body_is_qualified_through_its_enclosing_chain() {
    let located = locate_local(NESTED_IN_A_LAMBDA, "Config", 6).expect("local class located");

    let observed = (
        located.package.clone(),
        located.ancestors.clone(),
        located.declaration.kind,
        located.declaration.name.clone(),
    );

    assert_eq!(
        observed,
        (
            Some("io.ktor.server.application".to_string()),
            vec![
                "ApplicationPluginTest".to_string(),
                "test_routing_scoped_install".to_string(),
            ],
            DeclKind::Class,
            "Config".to_string(),
        )
    );
}

const TOP_LEVEL_AND_MEMBER: &str = r#"
package demo

class Outer {
    fun member() {}
    class Nested
}

fun topLevel() {}
"#;

/// Only a declaration inside a function or property body is local. A top-level declaration, a
/// member function and a member class are all reachable by the skeleton, so the fallback declines
/// them and leaves the skeleton's own answer in place.
#[test]
fn top_level_and_member_declarations_are_not_reported_as_local() {
    let observed = (
        locate_local(TOP_LEVEL_AND_MEMBER, "Outer", 4).is_some(),
        locate_local(TOP_LEVEL_AND_MEMBER, "member", 5).is_some(),
        locate_local(TOP_LEVEL_AND_MEMBER, "Nested", 6).is_some(),
        locate_local(TOP_LEVEL_AND_MEMBER, "topLevel", 9).is_some(),
    );

    assert_eq!(observed, (false, false, false, false));
}
