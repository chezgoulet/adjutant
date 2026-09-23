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

    /// Stop every subscription task owned by `plugin_id` (disable/uninstall/
    /// reload rebinding). Missing id is a no-op.
    ///
    /// `abort()` alone is fire-and-forget — it cancels at the next await point, so
    /// a handler already inside its body can finish afterwards, and the reload
    /// path relies on this to keep two generations from handling one event. Each
    /// task is therefore awaited with a short bound: cancellation is immediate,
    /// the timeout only covers a handler that ignores it.
    pub async fn clear_plugin(&self, plugin_id: &str) {
        let handles = {
            let mut map = self.subs.lock().expect("event bus poisoned");
            map.remove(plugin_id)
        };
        if let Some(handles) = handles {
            for h in &handles {
                h.abort();
            }
            for h in handles {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(1), h).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn mk_event() -> Event {
        Event {
            id: 1,
            event_type: "demo.ping".into(),
            payload: serde_json::json!({}),
            source: "test".into(),
            timestamp: chrono::Utc::now(),
        }
    }

    /// What `disable`/`uninstall` and `enable` now rely on: clearing stops
    /// delivery, and re-subscribing resumes it. Without this, "disabled" plugins
    /// kept running their event handlers.
    #[tokio::test]
    async fn clear_plugin_stops_delivery_and_resubscribe_resumes() {
        let bus = EventBus::new();
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        let sub = || {
            let counter = counter.clone();
            EventSubscription::new(
                "demo.",
                adjutant_sdk::event_handler(move |_| {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                }),
            )
        };

        bus.subscribe("demo", sub());
        assert_eq!(bus.subscriber_ids(), vec!["demo".to_string()]);
        let _ = bus.sender().send(mk_event());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(seen.load(Ordering::SeqCst), 1, "subscribed handler runs");

        bus.clear_plugin("demo").await;
        assert!(bus.subscriber_ids().is_empty());
        let _ = bus.sender().send(mk_event());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(seen.load(Ordering::SeqCst), 1, "cleared plugin must not run");

        bus.subscribe("demo", sub());
        let _ = bus.sender().send(mk_event());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(seen.load(Ordering::SeqCst), 2, "re-subscribed handler runs");
        bus.shutdown();
    }
}
