//! Adjutant core server library. Used by the `adjutant` binary and by
//! integration tests.

pub mod config;
pub mod db;
pub mod events;
pub mod host;
pub mod permissions;
pub mod plugin_runtime;
pub mod server;

pub use server::build_app;
