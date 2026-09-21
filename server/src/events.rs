//! Event bus: broadcast dispatch (SPEC §5.4).
//!
//! The broadcast channel is in-process and ephemeral; durability lives in
//! `core.events`, written by `host::CoreEvents` *before* the send — so this
//! module owns only fan-out, not persistence. Subscriptions are registered by
//! the plugin runtime, which spawns one task per subscription.

use tokio::sync::broadcast;

use adjutant_sdk::{Event, EventSubscription};

/// Capacity of the in-process broadcast channel. Slow subscribers drop
/// (lagged) rather than back-pressure the publisher — persistence in
/// `core.events` is the durability guarantee, not the channel.
const BUS_CAPACITY: usize = 1024;

pub struct EventBus {
    tx: broadcast::Sender<Event>,
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        Self { tx, handles: Vec::new() }
    }

    pub fn sender(&self) -> broadcast::Sender<Event> {
        self.tx.clone()
    }

    /// Register a plugin subscription: one task, prefix-filtered dispatch.
    pub fn subscribe(&mut self, plugin_id: &str, sub: EventSubscription) {
        let plugin_id = plugin_id.to_string();
        let mut rx = self.tx.subscribe();
        let filter = sub.filter.clone();
        let dispatch_filter = filter.clone();
        let handler = sub.handler;
        let handle = tokio::spawn({
            let plugin_id = plugin_id.clone();
            async move {
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            let matched =
                                dispatch_filter == "*" || ev.event_type.starts_with(&dispatch_filter);
                            if !matched {
                                continue;
                            }
                            if let Err(e) = handler(ev).await {
                                tracing::warn!(
                                    plugin = %plugin_id,
                                    error = %e,
                                    "event handler failed"
                                );
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(plugin = %plugin_id, dropped = n, "event subscriber lagged");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        });
        self.handles.push(handle);
        tracing::debug!(plugin = %plugin_id, filter, "event subscription registered");
    }

    /// Abort all subscription tasks (graceful shutdown).
    pub fn shutdown(&mut self) {
        for h in self.handles.drain(..) {
            h.abort();
        }
    }
}

impl Drop for EventBus {
    fn drop(&mut self) {
        self.shutdown();
    }
}
