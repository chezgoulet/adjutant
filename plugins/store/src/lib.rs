//! # adjutant-store — the troop's shop (SPEC §7.16)
//!
//! What a troop sells, to whom, and at what price: a catalogue (uniforms, patches,
//! insignia, camp gear, event merchandise), prices with a sliding scale so cost
//! never decides who belongs, equipment rentals as a priced product, orders and
//! their completion, and comp sales — a commander-and-above authority to complete
//! an order at no charge, with the reason recorded and the zero amount visible in
//! the ledger.
//!
//! Three rules bind it before a line of it is written, all from
//! `docs/design/plugin-to-plugin.md`:
//!
//! * **It holds no money and keeps no books.** Payment is `stripe`'s (§7.13) and
//!   the ledger is `finance`'s (§7.5). A paid order is completed by calling
//!   `stripe` as the caller — forward the caller's credential so stripe's own gate
//!   re-decides — and the ledger entry stays finance's (§3.3).
//! * **Custody belongs to `equipment`.** A rental is a priced product here; the
//!   item, its condition and the open-checkout state machine stay in `equipment`
//!   (§3.5: hold the item id, do not replicate its facts). There is exactly one
//!   checkout state machine in this system and it is not in this crate.
//! * **A comp is not a discount, it is an authority.** Completing an order at no
//!   charge needs a grant (a `store:comp` permission), a reason, and a visible
//!   zero-amount record — an auditable act, not a price of zero.
//!
//! Scaffolded, not yet implemented. The plugin loads and answers no routes; the
//! real manifest (routes, migrations, permissions, subscriptions) replaces this.

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;

pub struct StorePlugin {
    ctx: OnceLock<PluginContext>,
}

impl StorePlugin {
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

impl Default for StorePlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AdjutantPlugin for StorePlugin {
    fn id(&self) -> &str {
        "store"
    }

    fn name(&self) -> &str {
        "Store"
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

export_plugin!(StorePlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_its_identity() {
        let p = StorePlugin::new();
        assert_eq!(p.id(), "store");
        assert!(p.routes().is_empty(), "scaffold should declare no routes yet");
    }
}
