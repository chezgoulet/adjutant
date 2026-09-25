//! # adjutant-archive — Archive
//!
//! Archive: Congress proceedings, minutes, full-text search, and troop timeline (SPEC 7.8).
//!
//! Scaffolded, not yet implemented. The plugin loads and answers no routes; the
//! real manifest (routes, migrations, permissions, subscriptions) replaces this.

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;

pub struct ArchivePlugin {
    ctx: OnceLock<PluginContext>,
}

impl ArchivePlugin {
    pub fn new() -> Self {
        Self { ctx: OnceLock::new() }
    }

    #[allow(dead_code)]
    fn ctx(&self) -> &PluginContext {
        self.ctx
            .get()
            .expect("core must call init() before routes()/subscriptions()")
    }
}

impl Default for ArchivePlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AdjutantPlugin for ArchivePlugin {
    fn id(&self) -> &str {
        "archive"
    }

    fn name(&self) -> &str {
        "Archive"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError> {
        let _ = self.ctx.set(ctx);
        Ok(())
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        Vec::new()
    }
}

export_plugin!(ArchivePlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_its_identity() {
        let p = ArchivePlugin::new();
        assert_eq!(p.id(), "archive");
        assert!(p.routes().is_empty(), "scaffold should declare no routes yet");
    }
}
