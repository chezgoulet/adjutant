//! # broken-init-fixture — a plugin that cannot load, on purpose
//!
//! The fixture `server/tests/plugin_load_semantics.rs` needs to probe the *failed
//! enable* path (issue #99): a plugin whose `init` returns an error, so
//! `enable_plugin` has a real load failure to refuse — a 409 carrying the reason,
//! the reason recorded on the record as `last_error`, and the durable flag never
//! written.
//!
//! ## Why failing in `init`, and not something cruder
//!
//! Every simpler way to make a load fail takes the test process with it:
//!
//! * **corrupting a library the process has already opened is invisible** —
//!   `dlopen` dedupes by path and a retired library stays mapped, so the enable
//!   cheerfully answers `200 {"loaded":true}` and the probe proves nothing;
//! * **truncating it into a non-ELF is `SIGBUS`** — `dlopen` maps the file and
//!   reads past its end;
//! * **loading a valid library under a different name is `SIGSEGV`** in symbol
//!   resolution, with two copies of one object in one process.
//!
//! All three were tried. A plugin that returns an error from `init` is none of
//! them: a valid cdylib, an ordinary plugin, one error. And because
//! `load_opened` runs `init` *before* migrations, permissions and routes, the
//! failure lands on the ordinary path rather than on a version gate — which makes
//! it a better probe than an ABI-mismatch fixture would have been.
//!
//! ## This is not a plugin
//!
//! It declares no routes, no migrations, no permissions and no subscriptions. It
//! exists to fail, and it is named so that the Dockerfile's `libadjutant_*.so`
//! glob does not pick it up — see the note in its `Cargo.toml`.

use adjutant_sdk::prelude::*;

/// The error text is a constant so the probe can assert on it rather than on a
/// substring that a rewording would break.
pub const FAILURE: &str = "fixture: init fails on purpose";

pub struct BrokenInit;

impl BrokenInit {
    /// `export_plugin!` constructs the plugin through this.
    pub fn new() -> Self {
        Self
    }
}

impl Default for BrokenInit {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AdjutantPlugin for BrokenInit {
    fn id(&self) -> &str {
        "broken_init"
    }

    fn name(&self) -> &str {
        "Broken Init (test fixture)"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, _ctx: PluginContext) -> Result<(), SdkError> {
        Err(SdkError::Internal(FAILURE.to_string()))
    }

    /// No routes, deliberately: this plugin must never serve anything, and the
    /// only thing worth asserting about it is that loading it fails.
    fn routes(&self) -> Vec<RouteDefinition> {
        Vec::new()
    }
}

export_plugin!(BrokenInit);
