//! Adjutant core server library. Used by the `adjutant` binary and by
//! integration tests.

pub mod audit;
pub mod backup;
pub mod backup_routes;
pub mod bootstrap;
pub mod cli;
pub mod config;
pub mod db;
pub mod events;
pub mod host;
pub mod identity;
pub mod middleware;
pub mod notifications;
pub mod outbox;
pub mod permissions;
pub mod plugin_choice;
pub mod plugin_choice_routes;
pub mod plugin_runtime;
pub mod scheduler;
pub mod schema;
pub mod scope_hierarchy;
pub mod server;
pub mod wasm;

pub use server::build_app;
