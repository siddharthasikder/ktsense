//! One test per declaration kind, plus the sweep over the fixture that the card's acceptance names.
//!
//! Each kind test renders the extracted skeleton and compares the whole string, so a single
//! assertion covers the kind, its modifiers, its signature and its nesting at once. Rendering is
//! already pinned by `ktsense-core`'s own tests, so a failure here is an extraction failure.

use ktsense_core::{render_skeleton, RenderOptions, MAX_NESTING_DEPTH};
use ktsense_syntax::extract;

fn skeleton_of(source: &str) -> String {
    let file = extract("probe.kt", source).expect("extract");
    render_skeleton(&file, &RenderOptions::default().with_private())
}

#[test]
fn extracts_a_class_with_a_primary_constructor_and_supertype() {
    let skeleton = skeleton_of(
        r#"
open class UserService(private val repo: UserRepository, val clock: Clock) : Service, Closeable {
    override val name: String = "user"
    fun createUser(email: String, displayName: String? = null): Outcome<User> = TODO()
}
"#,
    );

    assert_eq!(
        skeleton,
        concat!(
            "open class UserService(private val repo: UserRepository, val clock: Clock) : Service, Closeable {\n",
            "    override val name: String\n",
            "    fun createUser(email: String, displayName: String? = null): Outcome<User>\n",
            "}"
        )
    );
}

#[test]
fn extracts_an_interface_a_fun_interface_and_default_methods() {
    let skeleton = skeleton_of(
        r#"
interface Repository<out T> {
    val size: Int
    fun findAll(page: Int = 0, pageSize: Int = 20): List<T>
    fun isEmpty(): Boolean = size == 0
}

fun interface Clock { fun nowEpochMillis(): Long }
"#,
    );

    assert_eq!(
        skeleton,
        concat!(
            "interface Repository<out T> {\n",
            "    val size: Int\n",
            "    fun findAll(page: Int = 0, pageSize: Int = 20): List<T>\n",
            "    fun isEmpty(): Boolean\n",
            "}\n",
            "fun interface Clock { fun nowEpochMillis(): Long }"
        )
    );
}

#[test]
fn extracts_a_data_class_a_value_class_and_a_sealed_hierarchy() {
    let skeleton = skeleton_of(
        r#"
data class User(val id: UserId, val email: String, val roles: Set<Role> = emptySet())

@JvmInline
value class UserId(val raw: Long)

sealed interface Outcome<out T> {
    data class Success<out T>(val value: T) : Outcome<T>
    object Pending : Outcome<Nothing>
}
"#,
    );

    assert_eq!(
        skeleton,
        concat!(
            "data class User(val id: UserId, val email: String, val roles: Set<Role> = emptySet())\n",
            "value class UserId(val raw: Long)\n",
            "sealed interface Outcome<out T> {\n",
            "    data class Success<out T>(val value: T) : Outcome<T>\n",
            "    object Pending : Outcome<Nothing>\n",
            "}"
        )
    );
}

#[test]
fn extracts_an_enum_with_entries_a_member_and_a_companion() {
    let skeleton = skeleton_of(
        r#"
enum class Role(val weight: Int) {
    VIEWER(0),
    EDITOR(10),
    ADMIN(100);

    abstract fun canMutate(): Boolean

    companion object {
        fun heaviest(roles: Collection<Role>): Role = VIEWER
    }
}
"#,
    );

    assert_eq!(
        skeleton,
        concat!(
            "enum class Role(val weight: Int) {\n",
            "    VIEWER, EDITOR, ADMIN;\n",
            "    abstract fun canMutate(): Boolean\n",
            "    companion object { fun heaviest(roles: Collection<Role>): Role }\n",
            "}"
        )
    );
}

#[test]
fn extracts_objects_companions_nested_and_inner_classes() {
    let skeleton = skeleton_of(
        r#"
object ServiceRegistry {
    val summary: String by lazy { "" }
    fun register(service: Service) {}
}

class Holder private constructor(private val backing: Map<Long, User>) {
    constructor() : this(emptyMap())
    class Stats(val reads: Long)
    inner class Snapshot { val takenFrom: Int = 0 }
    private companion object { const val LIMIT: Int = 10 }
}
"#,
    );

    assert_eq!(
        skeleton,
        concat!(
            "object ServiceRegistry {\n",
            "    val summary: String\n",
            "    fun register(service: Service)\n",
            "}\n",
            "class Holder private constructor(private val backing: Map<Long, User>) {\n",
            "    constructor()\n",
            "    class Stats(val reads: Long)\n",
            "    inner class Snapshot { val takenFrom: Int }\n",
            "    private companion object { const val LIMIT: Int }\n",
            "}"
        )
    );
}

#[test]
fn extracts_annotation_classes_and_type_aliases() {
    let skeleton = skeleton_of(
        r#"
@Target(AnnotationTarget.CLASS)
annotation class Audited(val category: String = "default")

annotation class Sanitized

typealias UserPredicate = (User) -> Boolean
"#,
    );

    assert_eq!(
        skeleton,
        concat!(
            "annotation class Audited(val category: String = \"default\")\n",
            "annotation class Sanitized\n",
            "typealias UserPredicate = (User) -> Boolean"
        )
    );
}

#[test]
fn extracts_top_level_functions_with_generics_varargs_receivers_and_constraints() {
    let skeleton = skeleton_of(
        r#"
suspend fun <T : Any> retrying(attempts: Int = 3, block: suspend () -> T): Outcome<T> = TODO()

fun User.hasAnyEmailDomain(vararg domains: String, ignoreCase: Boolean = true): Boolean = true

operator fun Page<Int>.plus(other: Int): Int = 0

inline fun <reified T : Service> ServiceRegistry.findOrNull(): T? = null

class Validator<T> where T : Any {
    fun <R : Comparable<R>> sortedBy(candidates: List<T>, selector: (T) -> R?): List<T> = candidates
}
"#,
    );

    assert_eq!(
        skeleton,
        concat!(
            "suspend fun <T : Any> retrying(attempts: Int = 3, block: suspend () -> T): Outcome<T>\n",
            "fun User.hasAnyEmailDomain(vararg domains: String, ignoreCase: Boolean = true): Boolean\n",
            "operator fun Page<Int>.plus(other: Int): Int\n",
            "inline fun <reified T : Service> ServiceRegistry.findOrNull(): T?\n",
            "class Validator<T> where T : Any {\n",
            "    fun <R : Comparable<R>> sortedBy(candidates: List<T>, selector: (T) -> R?): List<T>\n",
            "}"
        )
    );
}

#[test]
fn extracts_properties_with_visibility_getters_delegates_and_extension_receivers() {
    let skeleton = skeleton_of(
        r#"
private const val SALT: String = "ktsense"

val defaultPredicate: UserPredicate = { true }

var mutableCount: Int = 0

val User.initials: String
    get() = email.take(2)

internal val lazySummary: String by lazy { "" }
"#,
    );

    assert_eq!(
        skeleton,
        concat!(
            "private const val SALT: String\n",
            "val defaultPredicate: UserPredicate\n",
            "var mutableCount: Int\n",
            "val User.initials: String\n",
            "internal val lazySummary: String"
        )
    );
}

#[test]
fn an_extension_is_named_by_its_simple_name_with_the_receiver_shown_in_the_signature_only() {
    // source, simple name, receiver, rendered signature. The name is receiver-free so a lookup
    // matches what an occurrence spells, while the signature still shows the receiver (KT-121).
    let cases = [
        (
            "fun String?.blankToNull(): String? = null",
            "blankToNull",
            "String?",
            "fun String?.blankToNull(): String?",
        ),
        (
            "fun <T> List<T>.second(): T = this[1]",
            "second",
            "List<T>",
            "fun <T> List<T>.second(): T",
        ),
        (
            "val Foo.bar: Int get() = 0",
            "bar",
            "Foo",
            "val Foo.bar: Int",
        ),
        ("fun a.b.C.ext() {}", "ext", "a.b.C", "fun a.b.C.ext()"),
        (
            "suspend fun Flow<Int>.x(): Int = 0",
            "x",
            "Flow<Int>",
            "suspend fun Flow<Int>.x(): Int",
        ),
    ];

    let observed: Vec<(String, Option<String>, String)> = cases
        .iter()
        .map(|(source, ..)| {
            let file = extract("probe.kt", source).expect("extract");
            let declaration = file.declarations.first().expect("one declaration").clone();
            (
                declaration.name,
                declaration.receiver,
                render_skeleton(&file, &RenderOptions::default().with_private()),
            )
        })
        .collect();

    let expected: Vec<(String, Option<String>, String)> = cases
        .iter()
        .map(|(_, name, receiver, signature)| {
            (
                name.to_string(),
                Some(receiver.to_string()),
                signature.to_string(),
            )
        })
        .collect();

    assert_eq!(observed, expected);
}

#[test]
fn package_imports_and_kdoc_summaries_survive_extraction() {
    let source = r#"
package app.service

import app.domain.User
import app.util.Clock

/**
 * Application service for user lifecycle.
 *
 * Longer prose that must not reach the summary.
 *
 * @param repo the store
 */
class UserService(private val repo: UserRepository)
"#;
    let file = extract("app/service/UserService.kt", source).expect("extract");

    let observed = (
        file.package.clone(),
        file.imports.clone(),
        file.declarations[0].doc.clone(),
    );

    assert_eq!(
        observed,
        (
            Some("app.service".to_string()),
            vec!["app.domain.User".to_string(), "app.util.Clock".to_string()],
            Some("Application service for user lifecycle.".to_string()),
        )
    );
}

#[test]
fn inferred_types_are_marked_unknown_while_genuine_unit_and_written_types_are_not() {
    let skeleton = skeleton_of(
        r#"
val inferred = 3
val typed: Int = 3
fun expressionBody() = compute()
fun blockBody() { compute() }
fun explicit(): Int = compute()
interface Contract { fun abstractMethod() }
"#,
    );

    assert_eq!(
        skeleton,
        concat!(
            "val inferred /* inferred */\n",
            "val typed: Int\n",
            "fun expressionBody() /* inferred */\n",
            "fun blockBody()\n",
            "fun explicit(): Int\n",
            "interface Contract { fun abstractMethod() }"
        )
    );
}

#[test]
fn a_generic_typealias_keeps_its_type_parameters() {
    let skeleton = skeleton_of("typealias Pred<T> = (T) -> Boolean\n");

    assert_eq!(skeleton, "typealias Pred<T> = (T) -> Boolean");
}

#[test]
fn nesting_past_the_depth_limit_truncates_instead_of_overflowing_the_stack() {
    let requested_depth = 2000;
    let mut source = String::new();
    for level in 0..requested_depth {
        source.push_str(&format!("class C{level} {{\n"));
    }
    source.push_str(&"}".repeat(requested_depth));

    let file = extract("deep.kt", &source).expect("extract");

    let observed = (file.truncated, file.declaration_count());
    assert_eq!(observed, (true, MAX_NESTING_DEPTH + 1));
}

/// Annotations are extracted for a class, a nested function and a class annotation split across
/// several source lines, the last normalized to a single line. Rendering drops annotations by
/// default, so this asserts the extracted field directly rather than through the skeleton text.
#[test]
fn extracts_class_function_and_multiline_annotations_each_on_one_line() {
    let file = extract(
        "probe.kt",
        concat!(
            "@Component(modules = [AwsModule::class, ConfigModule::class])\n",
            "class Wiring {\n",
            "    @JvmStatic\n",
            "    fun boot(): Unit = TODO()\n",
            "}\n",
            "\n",
            "@Retention(\n",
            "    AnnotationRetention.RUNTIME,\n",
            ")\n",
            "annotation class Audited\n",
        ),
    )
    .expect("extract");

    let wiring = &file.declarations[0];
    let boot = &wiring.children[0];
    let audited = &file.declarations[1];

    assert_eq!(
        (
            wiring.annotations.clone(),
            boot.annotations.clone(),
            audited.annotations.clone(),
        ),
        (
            vec!["@Component(modules = [AwsModule::class, ConfigModule::class])".to_string()],
            vec!["@JvmStatic".to_string()],
            vec!["@Retention(AnnotationRetention.RUNTIME)".to_string()],
        )
    );
}

#[test]
fn folds_a_multi_line_class_header_and_annotation_onto_one_line() {
    let file = extract(
        "probe.kt",
        r#"
@Component(
    modules = [
        JacksonModule::class,
        ApiClientModule::class,
    ],
)
open class ApplicationCallPipeline(
    developmentMode: Boolean,
) : Pipeline<Unit, PipelineCall>(
    Setup,
    Monitoring,
    Fallback
) {
    val environment: String = "x"
}
"#,
    )
    .expect("extract");
    let skeleton = render_skeleton(
        &file,
        &RenderOptions::default().with_private().with_annotations(),
    );

    assert_eq!(
        skeleton,
        concat!(
            "@Component(modules = [JacksonModule::class, ApiClientModule::class])\n",
            "open class ApplicationCallPipeline(developmentMode: Boolean) : Pipeline<Unit, PipelineCall>(Setup, Monitoring, Fallback) {\n",
            "    val environment: String\n",
            "}"
        )
    );
}
