//! Event bus: broadcast dispatch (SPEC §5.4).
//!
//! Fan-out only — durability lives in `core.events`, written by
//! `host::CoreEvents` *before* the send. Subscriptions are tracked **per
//! plugin** so a hot-reload can abort a plugin's old tasks before rebinding
//! the new instance (otherwise both generations would handle the same event).

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::broadcast;

use adjutant_sdk::{Event, EventSubscription};

/// Capacity of the in-process broadcast channel. Slow subscribers drop
/// (lagged) rather than back-pressure the publisher — persistence in
/// `core.events` is the durability guarantee, not the channel.
const BUS_CAPACITY: usize = 1024;

pub struct EventBus {
    tx: broadcast::Sender<Event>,
    /// plugin_id → subscription tasks. Mutex (not RwLock): never held across
    /// an await — spawn/abort are synchronous.
    subs: Mutex<HashMap<String, Vec<tokio::task::JoinHandle<()>>>>,
}

impl EventBus {
    pub fn new() -> std::sync::Arc<Self> {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        std::sync::Arc::new(Self { tx, subs: Mutex::new(HashMap::new()) })
    }

    pub fn sender(&self) -> broadcast::Sender<Event> {
        self.tx.clone()
    }

    /// Bind one subscription task for `plugin_id`, replacing nothing (call
    /// [`Self::clear_plugin`] first when rebinding an existing plugin).
    pub fn subscribe(&self, plugin_id: &str, sub: EventSubscription) {
        let mut rx = self.tx.subscribe();
        let filter_for_log = sub.filter.clone(); // stays for the log line below
        let filter = sub.filter; // moved into the dispatch task
        let handler = sub.handler;
        let owner = plugin_id.to_string();
        let log_id = plugin_id.to_string();

        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if !(filter == "*" || ev.event_type.starts_with(&filter)) {
                            continue;
                        }
                        if let Err(e) = handler(ev).await {
                            tracing::warn!(plugin = %log_id, error = %e, "event handler failed");
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(plugin = %log_id, dropped = n, "event subscriber lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let mut map = self.subs.lock().expect("event bus poisoned");
        map.entry(owner).or_default().push(handle);
        tracing::debug!(plugin = plugin_id, filter = %filter_for_log, "event subscription registered");
    }

    /// Abort every subscription task owned by `plugin_id` (disable/uninstall/
    /// reload rebinding). Missing id is a no-op.
    pub fn clear_plugin(&self, plugin_id: &str) {
        let mut map = self.subs.lock().expect("event bus poisoned");
        if let Some(handles) = map.remove(plugin_id) {
            for h in handles {
                h.abort();
            }
            tracing::debug!(plugin = plugin_id, "event subscriptions cleared");
        }
    }

    /// Currently-bound plugin ids.
    pub fn subscriber_ids(&self) -> Vec<String> {
        self.subs
            .lock()
            .expect("event bus poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// Abort everything (graceful shutdown).
    pub fn shutdown(&self) {
        let mut map = self.subs.lock().expect("event bus poisoned");
        for handles in map.values_mut() {
            for h in handles.drain(..) {
                h.abort();
            }
        }
        map.clear();
    }
}

impl Default for EventBus {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        Self { tx, subs: Mutex::new(HashMap::new()) }
    }
}
