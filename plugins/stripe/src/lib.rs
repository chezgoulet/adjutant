//! # adjutant-stripe — payments for a troop's own things (SPEC §7.13)
//!
//! SPEC §7.13 gives this plugin the payment path: dues collection, fundraising
//! donations, event fees, and the webhook that confirms a payment happened. It is
//! a per-troop integration — a troop chooses whether to run it — and it is the
//! only place a card is ever charged.
//!
//! Two rules bind it before a line of it is written, both in
//! `docs/design/boundary.md`:
//!
//! * **Finance owns the ledger.** A confirmed payment reaches
//!   `finance.transactions` by calling finance's API **as the caller**, with the
//!   caller's credential forwarded so finance's own gate re-decides — never by a
//!   privileged internal call, and never by writing another plugin's schema.
//! * **The money path cannot be event-only.** A charged card with a failed
//!   ledger write is money moved with no record, in a troop whose Accords
//!   mandate open books. Events may notify; they may not be how the ledger
//!   learns to write.
//!
//! Scaffolded, not yet implemented. The plugin loads and answers no routes; the
//! real manifest (routes, migrations, permissions, subscriptions) replaces this.

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;

pub struct StripePlugin {
    ctx: OnceLock<PluginContext>,
}

impl StripePlugin {
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

impl Default for StripePlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AdjutantPlugin for StripePlugin {
    fn id(&self) -> &str {
        "stripe"
    }

    fn name(&self) -> &str {
        "Stripe"
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

export_plugin!(StripePlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_its_identity() {
        let p = StripePlugin::new();
        assert_eq!(p.id(), "stripe");
        assert!(p.routes().is_empty(), "scaffold should declare no routes yet");
    }
}
