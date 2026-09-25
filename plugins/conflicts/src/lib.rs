//! # adjutant-conflicts — Conflicts
//!
//! Conflicts: the staged resolution pathway and case management (SPEC 7.9).
//!
//! Scaffolded, not yet implemented. The plugin loads and answers no routes; the
//! real manifest (routes, migrations, permissions, subscriptions) replaces this.

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;

pub struct ConflictsPlugin {
    ctx: OnceLock<PluginContext>,
}

impl ConflictsPlugin {
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

impl Default for ConflictsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AdjutantPlugin for ConflictsPlugin {
    fn id(&self) -> &str {
        "conflicts"
    }

    fn name(&self) -> &str {
        "Conflicts"
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

export_plugin!(ConflictsPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_its_identity() {
        let p = ConflictsPlugin::new();
        assert_eq!(p.id(), "conflicts");
        assert!(p.routes().is_empty(), "scaffold should declare no routes yet");
    }
}
