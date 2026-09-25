//! Adjutant core server library. Used by the `adjutant` binary and by
//! integration tests.

pub mod config;
pub mod cli;
pub mod db;
pub mod events;
pub mod host;
pub mod identity;
pub mod middleware;
pub mod outbox;
pub mod permissions;
pub mod plugin_runtime;
pub mod schema;
pub mod scheduler;
pub mod scope_hierarchy;
pub mod server;
pub mod wasm;

pub use server::build_app;
