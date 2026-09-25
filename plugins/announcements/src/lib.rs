//! # adjutant-announcements — Announcements
//!
//! Announcements: troop communication, categories, and read receipts (SPEC 7.14).
//!
//! Scaffolded, not yet implemented. The plugin loads and answers no routes; the
//! real manifest (routes, migrations, permissions, subscriptions) replaces this.

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;

pub struct AnnouncementsPlugin {
    ctx: OnceLock<PluginContext>,
}

impl AnnouncementsPlugin {
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

impl Default for AnnouncementsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AdjutantPlugin for AnnouncementsPlugin {
    fn id(&self) -> &str {
        "announcements"
    }

    fn name(&self) -> &str {
        "Announcements"
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

export_plugin!(AnnouncementsPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_its_identity() {
        let p = AnnouncementsPlugin::new();
        assert_eq!(p.id(), "announcements");
        assert!(p.routes().is_empty(), "scaffold should declare no routes yet");
    }
}
