//! Subtypes of a Java class or interface a `trace` resolved to (KT-116).
//!
//! The engine resolves Kotlin only, so a Java type has no implementors it can answer. This scans the
//! workspace's `.java` and `.kt` sources for types that name the queried type in a supertype clause,
//! matched by simple name, and hands them to `ktsense-core` to close transitively. Java supertypes
//! come from the pure KT-114 scan (which already reduces them to simple names), Kotlin ones from the
//! skeleton's supertype list reduced here; the pure crate owns the closure, this reads the files.

use std::path::{Path, PathBuf};

use ktsense_core::{
    java_declarations, java_package, resolve_supertype_implementors, DeclKind, Declaration,
    Definition, JavaDeclKind, SupertypeImplementors, TypeNode,
};

use crate::{collect_java_files, collect_kotlin_files, normalized_path, CommandError};

/// The subtypes of the traced definition when it is a Java class or interface, or `None` when it is
/// not (a Kotlin type keeps the engine's implementors, and a Java enum, record, annotation, method
/// or field has no subtype listing). A Java class or interface with no subtypes still returns an
/// empty listing, so the `## Implementors` section states the text-match precision for it.
pub(crate) fn supertype_implementors(
    root: &Path,
    definition: &Definition,
) -> Result<Option<SupertypeImplementors>, CommandError> {
    if !definition.path.ends_with(".java") {
        return Ok(None);
    }
    let target = ktsense_core::last_segment(&definition.qualified_name).to_string();
    if !java_definition_is_class_or_interface(root, definition, &target) {
        return Ok(None);
    }
    let nodes = collect_type_nodes(root, definition)?;
    Ok(Some(resolve_supertype_implementors(&target, &nodes)))
}

/// Whether the traced Java definition is a class or interface, read from its own file's declaration
/// scan. The declaration at the definition's name and line is preferred, falling back to the first
/// same-named declaration when the engine and the scan disagree on the line, so a multi-line header
/// still resolves. An enum, record or annotation type, a method or field, or a file that cannot be
/// read is answered `false`, so only a class or interface gains a supertype-matched implementor list.
fn java_definition_is_class_or_interface(
    root: &Path,
    definition: &Definition,
    target: &str,
) -> bool {
    let absolute = if Path::new(&definition.path).is_absolute() {
        PathBuf::from(&definition.path)
    } else {
        root.join(&definition.path)
    };
    let Ok(source) = std::fs::read_to_string(&absolute) else {
        return false;
    };
    let declarations = java_declarations(&source);
    let found = declarations
        .iter()
        .find(|declaration| declaration.name == target && declaration.line == definition.line)
        .or_else(|| {
            declarations
                .iter()
                .find(|declaration| declaration.name == target)
        });
    matches!(
        found.map(|declaration| declaration.kind),
        Some(JavaDeclKind::Class | JavaDeclKind::Interface)
    )
}

/// Every type declared in the workspace's `.java` and `.kt` sources, as a [`TypeNode`] carrying its
/// supertype simple names, excluding the queried definition itself so it is never its own subtype.
fn collect_type_nodes(root: &Path, definition: &Definition) -> Result<Vec<TypeNode>, CommandError> {
    let mut nodes = Vec::new();
    for path in collect_java_files(root)? {
        collect_java_type_nodes(root, &path, &mut nodes);
    }
    for path in collect_kotlin_files(root)? {
        collect_kotlin_type_nodes(root, &path, &mut nodes);
    }
    nodes.retain(|node| !(node.path == definition.path && node.line == definition.line));
    Ok(nodes)
}

/// Appends every Java type in one file as a [`TypeNode`]. The KT-114 scan already reduces each
/// supertype to its simple name. A file that cannot be read contributes nothing.
fn collect_java_type_nodes(root: &Path, path: &Path, nodes: &mut Vec<TypeNode>) {
    let Ok(source) = std::fs::read_to_string(path) else {
        return;
    };
    let package = java_package(&source);
    let display = normalized_path(root, path);
    for declaration in java_declarations(&source) {
        if !declaration.kind.is_type() {
            continue;
        }
        nodes.push(TypeNode {
            fqn: join_fqn(
                package.as_deref(),
                &declaration.enclosing,
                &declaration.name,
            ),
            simple_name: declaration.name,
            path: display.clone(),
            line: declaration.line,
            supertypes: declaration.supertypes,
        });
    }
}

/// Appends every Kotlin type in one file as a [`TypeNode`], reducing each declared supertype to its
/// simple name so it matches a Java or Kotlin parent by name. A file that cannot be read or parsed
/// contributes nothing.
fn collect_kotlin_type_nodes(root: &Path, path: &Path, nodes: &mut Vec<TypeNode>) {
    let Ok(source) = std::fs::read_to_string(path) else {
        return;
    };
    let display = normalized_path(root, path);
    let Ok(skeleton) = ktsense_syntax::extract(display.clone(), &source) else {
        return;
    };
    let mut ancestors = Vec::new();
    gather_kotlin_types(
        skeleton.package.as_deref(),
        &display,
        &skeleton.declarations,
        &mut ancestors,
        nodes,
    );
}

fn gather_kotlin_types(
    package: Option<&str>,
    path: &str,
    declarations: &[Declaration],
    ancestors: &mut Vec<String>,
    nodes: &mut Vec<TypeNode>,
) {
    for declaration in declarations {
        if is_kotlin_type(declaration.kind) && !declaration.supertypes.is_empty() {
            nodes.push(TypeNode {
                fqn: join_fqn(package, ancestors, &declaration.name),
                simple_name: declaration.name.clone(),
                path: path.to_string(),
                line: declaration.line,
                supertypes: declaration
                    .supertypes
                    .iter()
                    .map(|supertype| simple_supertype_name(supertype))
                    .collect(),
            });
        }
        ancestors.push(declaration.name.clone());
        gather_kotlin_types(package, path, &declaration.children, ancestors, nodes);
        ancestors.pop();
    }
}

fn is_kotlin_type(kind: DeclKind) -> bool {
    matches!(
        kind,
        DeclKind::Class | DeclKind::Interface | DeclKind::Object
    )
}

/// The simple name of a Kotlin supertype specifier: its base type without generic arguments, a
/// constructor call's parentheses, a `by` delegation tail, or a package qualifier. `a.b.Foo<Bar>`
/// and `Foo()` both reduce to `Foo`.
fn simple_supertype_name(specifier: &str) -> String {
    let base = specifier
        .split(['<', '(', ' ', '\t', '\n'])
        .next()
        .unwrap_or(specifier);
    base.rsplit('.').next().unwrap_or(base).to_string()
}

fn join_fqn(package: Option<&str>, ancestors: &[String], name: &str) -> String {
    package
        .into_iter()
        .map(str::to_string)
        .chain(ancestors.iter().filter(|part| !part.is_empty()).cloned())
        .chain(std::iter::once(name.to_string()))
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Java base with a direct Java subtype and a transitive Kotlin subtype: the scan lists the
    /// Java subtype direct, the Kotlin subtype via the Java one, each with its FQN and location, and
    /// excludes the base itself. Composed into one value and asserted once.
    #[test]
    fn a_java_base_lists_its_java_and_kotlin_subtypes_direct_then_transitive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join("Base.java"),
            "package pkg;\npublic abstract class Base {\n}\n",
        )
        .expect("write base");
        std::fs::write(
            root.join("Mid.java"),
            "package pkg;\npublic class Mid extends Base {\n}\n",
        )
        .expect("write mid");
        std::fs::write(root.join("Leaf.kt"), "package pkg\n\nclass Leaf : Mid()\n")
            .expect("write leaf");
        let definition = Definition {
            qualified_name: "pkg.Base".to_string(),
            path: "Base.java".to_string(),
            line: 2,
            signature: "public abstract class Base".to_string(),
        };

        let answer = supertype_implementors(root, &definition)
            .expect("scan")
            .expect("java class gets a listing");

        let observed: Vec<(String, String, u32, Option<String>)> = answer
            .implementors
            .iter()
            .map(|implementor| {
                (
                    implementor.fqn.clone(),
                    implementor.path.clone(),
                    implementor.line,
                    implementor.via.clone(),
                )
            })
            .collect();
        assert_eq!(
            (answer.precision, observed),
            (
                "text match (supertypes matched by name)",
                vec![
                    ("pkg.Mid".to_string(), "Mid.java".to_string(), 2, None),
                    (
                        "pkg.Leaf".to_string(),
                        "Leaf.kt".to_string(),
                        3,
                        Some("Mid".to_string())
                    ),
                ],
            )
        );
    }

    /// A Kotlin-declared definition keeps the engine's implementors (returns `None`), so a Kotlin
    /// type's implementors are unchanged by this scan.
    #[test]
    fn a_kotlin_definition_is_left_to_the_engine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let definition = Definition {
            qualified_name: "pkg.Base".to_string(),
            path: "Base.kt".to_string(),
            line: 1,
            signature: "interface Base".to_string(),
        };

        let answer = supertype_implementors(dir.path(), &definition).expect("scan");

        assert!(answer.is_none());
    }
}
