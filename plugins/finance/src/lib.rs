//! # adjutant-finance — Finance
//!
//! Finance: funds, transactions, budgets vs actuals, and sliding-scale dues (SPEC 7.5).
//!
//! Scaffolded, not yet implemented. The plugin loads and answers no routes; the
//! real manifest (routes, migrations, permissions, subscriptions) replaces this.

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;

pub struct FinancePlugin {
    ctx: OnceLock<PluginContext>,
}

impl FinancePlugin {
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

impl Default for FinancePlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AdjutantPlugin for FinancePlugin {
    fn id(&self) -> &str {
        "finance"
    }

    fn name(&self) -> &str {
        "Finance"
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

export_plugin!(FinancePlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_its_identity() {
        let p = FinancePlugin::new();
        assert_eq!(p.id(), "finance");
        assert!(p.routes().is_empty(), "scaffold should declare no routes yet");
    }
}
