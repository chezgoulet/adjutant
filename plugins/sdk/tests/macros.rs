//! The SDK's declaration macros, exercised **from a file that is not the one
//! that defines them**.
//!
//! That is the load-bearing part. `include_str!` inside a `macro_rules!`
//! expansion is resolved when the macro is *invoked*, not where it was defined —
//! so a plugin's `migrations! { 1 => "x" => "../migrations/001_x.sql" }` in
//! `src/lib.rs` reads the SQL in *that plugin's* directory. This file proves the
//! resolution rule from the other direction: its fixture paths are relative to
//! `tests/`, and a path relative to the SDK's `src/lib.rs` (where the macro is
//! defined) would not resolve at all. This file compiling is the proof.
//!
//! The fixtures are deliberately two files with distinguishable contents, so the
//! tests below can assert that each version is bound to *its* file rather than to
//! any file.

use adjutant_sdk::prelude::*;
use adjutant_sdk::testing::*;

/// The vocabulary of a plugin, declared once.
pub mod perms {
    adjutant_sdk::permissions! {
        /// Read greetings.
        READ = "greetings:read" => "Read greetings";
        /// Manage greetings.
        MANAGE = "greetings:manage" => "Manage greetings";
    }
}

/// The migrations of a plugin: SQL in files, version and name bound to them.
pub mod migrations {
    adjutant_sdk::migrations! {
        1 => "greetings_schema" => "fixtures/001_greetings_schema.sql";
        2 => "greetings_index" => "fixtures/002_greetings_index.sql";
    }
}

#[test]
fn permissions_macro_declares_the_vocabulary_once() {
    // Both halves of each declaration come from the same literal — there is no
    // second copy for a description to drift against.
    assert_eq!(perms::READ.id, "greetings:read");
    assert_eq!(perms::READ.description, "Read greetings");
    assert_eq!(perms::MANAGE, PermissionDecl::new("greetings:manage", "Manage greetings"));

    assert_eq!(perms::ALL.len(), 2);
    assert_eq!(perms::SET.ids(), vec!["greetings:read", "greetings:manage"]);
    assert_eq!(perms::SET.vocabulary(), "greetings:read, greetings:manage");
    assert!(perms::SET.has("greetings:manage"));
    assert!(!perms::SET.has("greetings:write"));

    // The declaration as the core reads it, in declaration order.
    let granted = perms::granted();
    assert_eq!(granted.len(), 2);
    assert_eq!(granted[0].id, "greetings:read");
    assert_eq!(granted[0].description, "Read greetings");
    assert_eq!(granted[1].id, "greetings:manage");
}

#[test]
fn migrations_macro_binds_each_file_to_its_version_and_name() {
    let sources = migrations::MIGRATIONS;
    assert_eq!(sources.len(), 2);
    assert_eq!((sources[0].version, sources[0].name), (1, "greetings_schema"));
    assert_eq!((sources[1].version, sources[1].name), (2, "greetings_index"));

    // The SQL arrives from the file beside this test file, and each migration
    // carries its own — the table statement is in the first, the index in the
    // second, and neither is in both.
    assert!(
        sources[0].sql.contains("CREATE TABLE IF NOT EXISTS greetings"),
        "migration 1 did not embed its file: {:?}",
        sources[0].sql
    );
    assert!(sources[1].sql.contains("idx_greetings_message"));
    assert!(
        !sources[0].sql.contains("idx_greetings_message"),
        "migration 1 embedded migration 2's file"
    );

    // `all()` is what `AdjutantPlugin::migrations` returns, in order.
    let all = migrations::all();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].version, 1);
    assert_eq!(all[0].name, "greetings_schema");
    assert_eq!(all[1].sql, sources[1].sql);
    assert_eq!(migrations::find(2).map(|m| m.name), Some("greetings_index"));
    assert!(migrations::find(3).is_none());
}

/// The generated per-crate assertion: every gate names a declared permission.
#[test]
fn generated_assertion_accepts_a_gate_on_a_declared_permission() {
    let routes = vec![
        RouteDefinition::get_protected(
            "/api/greetings/things",
            perms::READ.id,
            route_handler(|_| async { PluginResponse::json(200, &serde_json::json!({})) }),
        ),
        // An ungated route is a decision, not a mistake: not reported.
        RouteDefinition::get(
            "/api/greetings/health",
            route_handler(|_| async { PluginResponse::json(200, &serde_json::json!({})) }),
        ),
    ];
    perms::assert_routes_gate_declared(&routes);
    assert!(undeclared_route_gates(perms::ALL, &routes).is_empty());
}

/// The failure the core would report at load (`route … requires permission '…'
/// which the plugin does not grant`), reported as a failing test instead.
#[test]
#[should_panic(expected = "greetings:reed")]
fn generated_assertion_names_an_undeclared_gate() {
    let routes = vec![RouteDefinition::post_protected(
        "/api/greetings/things",
        "greetings:reed",
        route_handler(|_| async { PluginResponse::json(200, &serde_json::json!({})) }),
    )];
    perms::assert_routes_gate_declared(&routes);
}

/// The `&str` comparison the macros' compile-time checks are built on: `PartialEq
/// for str` is not const, so byte equality is spelled out.
#[test]
fn const_str_eq_is_equality() {
    use adjutant_sdk::const_str_eq;
    assert!(const_str_eq("", ""));
    assert!(const_str_eq("greetings:read", "greetings:read"));
    assert!(!const_str_eq("greetings:read", "greetings:readd"));
    assert!(!const_str_eq("a", ""));
    assert!(!const_str_eq("", "a"));
    assert!(!const_str_eq("greetings:read", "greetings:READ"));
}

/// The helper is `const`, which is the whole reason it exists: the macros'
/// uniqueness checks are `const` evaluation, not runtime assertions. A non-const
/// comparison would make the two lines below a build error (`E0015: cannot call
/// non-const operator in constants`), which is exactly what `PartialEq for str`
/// gives you.
#[test]
fn const_str_eq_is_usable_in_const_context() {
    use adjutant_sdk::const_str_eq;
    const _: () = assert!(const_str_eq("greetings:read", "greetings:read"));
    const _: () = assert!(!const_str_eq("greetings:read", "greetings:readd"));

    // And it agrees with `==` on the values the macros compare.
    let (a, b, c) = ("greetings:read", "greetings:read", "greetings:readd");
    assert_eq!(const_str_eq(a, b), a == b);
    assert_eq!(const_str_eq(a, c), a == c);
}
