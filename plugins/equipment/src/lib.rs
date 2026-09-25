//! # adjutant-equipment — Equipment
//!
//! Equipment: inventory, checkout/checkin, and maintenance schedules (SPEC 7.6).
//!
//! Scaffolded, not yet implemented. The plugin loads and answers no routes; the
//! real manifest (routes, migrations, permissions, subscriptions) replaces this.

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;

pub struct EquipmentPlugin {
    ctx: OnceLock<PluginContext>,
}

impl EquipmentPlugin {
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

impl Default for EquipmentPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AdjutantPlugin for EquipmentPlugin {
    fn id(&self) -> &str {
        "equipment"
    }

    fn name(&self) -> &str {
        "Equipment"
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

export_plugin!(EquipmentPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_its_identity() {
        let p = EquipmentPlugin::new();
        assert_eq!(p.id(), "equipment");
        assert!(p.routes().is_empty(), "scaffold should declare no routes yet");
    }
}
