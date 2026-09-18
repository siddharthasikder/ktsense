//! One test per declaration kind, plus the sweep over the fixture that the card's acceptance names.
//!
//! Each kind test renders the extracted skeleton and compares the whole string, so a single
//! assertion covers the kind, its modifiers, its signature and its nesting at once. Rendering is
//! already pinned by `ktsense-core`'s own tests, so a failure here is an extraction failure.

use ktsense_core::{render_skeleton, RenderOptions};
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
