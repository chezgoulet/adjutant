//! Request identity resolution: plugin-registered providers first, dev-header
//! stub second (SPEC §7.1 — the auth plugin replaces the stub).
//!
//! Providers register under their plugin id (`IdentityRegistrar`). A provider
//! registered by a *disabled* plugin stops answering (disable = stop serving);
//! uninstall removes it; hot-reload replaces it by owner key, and entries whose
//! owner is no longer live are pruned after the swap.
//!
//! Resolution order in `dispatch`:
//! 1. enabled providers — first `Some(identity)` wins; a provider `Err` logs
//!    and is skipped (never escalates, never falls through to spoofable headers
//!    *because of* an error — the stub is gated by `allow_dev_headers` alone);
//! 2. dev headers, only when `auth.allow_dev_headers` (default on in dev;
//!    MUST be off in production — it is trivially spoofable).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use adjutant_sdk::{Identity, IdentityProvider, IdentityRegistrar};
use tokio::sync::RwLock;

pub struct IdentityHub {
    providers: RwLock<HashMap<String, Arc<dyn IdentityProvider>>>,
    disabled: RwLock<HashSet<String>>,
}

impl IdentityHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            providers: RwLock::new(HashMap::new()),
            disabled: RwLock::new(HashSet::new()),
        })
    }

    /// Plugin id → provider (replaces any previous registration by the same
    /// owner — that is what makes hot-reload swap providers atomically).
    pub fn register(&self, owner: &str, provider: Arc<dyn IdentityProvider>) {
        tracing::info!(plugin = owner, "identity provider registered");
        // Blocking lock is fine: register runs during plugin init, never
        // inside a request, and the critical section holds no awaits.
        self.providers
            .try_write()
            .expect("identity hub locked during init")
            .insert(owner.to_string(), provider);
    }

    pub fn set_enabled(&self, owner: &str, on: bool) {
        let mut dis = self.disabled.try_write().expect("identity hub locked");
        if on {
            dis.remove(owner);
        } else {
            dis.insert(owner.to_string());
            tracing::info!(plugin = owner, "identity provider disabled");
        }
    }

    pub fn remove(&self, owner: &str) {
        let mut p = self.providers.try_write().expect("identity hub locked");
        let mut d = self.disabled.try_write().expect("identity hub locked");
        d.remove(owner);
        if p.remove(owner).is_some() {
            tracing::info!(plugin = owner, "identity provider removed (uninstalled)");
        }
    }

    /// After a reload: drop providers whose owner is no longer in the live set.
    /// Live owners were (re)registered during `load_all`'s init, so pruning
    /// cannot remove a provider that just registered under the same key.
    pub fn retain(&self, live: &HashSet<String>) {
        let mut p = self.providers.try_write().expect("identity hub locked");
        let mut d = self.disabled.try_write().expect("identity hub locked");
        p.retain(|owner, _| live.contains(owner));
        d.retain(|owner| live.contains(owner));
    }

    /// True when `owner` is the *only* enabled provider and at least one exists.
    ///
    /// Used to refuse lifecycle actions that would leave the process with no way
    /// to authenticate anybody: with the dev-header stub off, the last provider
    /// going away makes every authenticated route (including the admin route
    /// that would undo the change) unreachable until a restart.
    pub fn is_sole_enabled_provider(&self, owner: &str) -> bool {
        let p = self.providers.try_read().expect("identity hub locked");
        let d = self.disabled.try_read().expect("identity hub locked");
        let mut enabled = p.keys().filter(|k| !d.contains(*k));
        matches!((enabled.next(), enabled.next()), (Some(first), None) if first == owner)
    }

    pub fn owners(&self) -> Vec<String> {
        self.providers
            .try_read()
            .expect("identity hub locked")
            .keys()
            .cloned()
            .collect()
    }

    /// Ask every enabled provider, in registration order (map order —
    /// providers must not be order-sensitive; first Some wins).
    pub async fn identify(&self, headers: &HashMap<String, String>) -> Option<Identity> {
        let snapshot: Vec<(String, Arc<dyn IdentityProvider>)> = {
            let p = self.providers.try_read().expect("identity hub locked");
            let d = self.disabled.try_read().expect("identity hub locked");
            p.iter()
                .filter(|(owner, _)| !d.contains(*owner))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        for (owner, provider) in snapshot {
            match provider.identify(headers).await {
                Ok(Some(id)) => return Some(id),
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(plugin = %owner, error = %e, "identity provider errored; skipping");
                }
            }
        }
        None
    }
}

impl IdentityRegistrar for IdentityHub {
    fn register(&self, owner: &str, provider: Arc<dyn IdentityProvider>) {
        IdentityHub::register(self, owner, provider);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adjutant_sdk::{async_trait, SdkError};

    struct HeaderStub;

    #[async_trait]
    impl IdentityProvider for HeaderStub {
        async fn identify(
            &self,
            headers: &HashMap<String, String>,
        ) -> Result<Option<Identity>, SdkError> {
            match headers.get("x-stub-user") {
                Some(u) => Ok(Some(Identity {
                    user_id: u.clone(),
                    roles: headers
                        .get("x-stub-role")
                        .map(|r| vec![r.clone()])
                        .unwrap_or_default(),
                })),
                None => Ok(None),
            }
        }
    }

    fn hdrs(user: Option<&str>, role: Option<&str>) -> HashMap<String, String> {
        let mut m = HashMap::new();
        if let Some(u) = user {
            m.insert("x-stub-user".into(), u.into());
        }
        if let Some(r) = role {
            m.insert("x-stub-role".into(), r.into());
        }
        m
    }

    #[tokio::test]
    async fn provider_answers_then_disabled_then_removed() {
        let hub = IdentityHub::new();
        hub.register("stub", Arc::new(HeaderStub));

        let id = hub.identify(&hdrs(Some("chris"), Some("chief"))).await;
        assert_eq!(id.as_ref().map(|i| i.user_id.as_str()), Some("chris"));
        assert!(hub.identify(&hdrs(None, None)).await.is_none());

        hub.set_enabled("stub", false);
        assert!(
            hub.identify(&hdrs(Some("chris"), None)).await.is_none(),
            "disabled provider must not answer"
        );

        hub.set_enabled("stub", true);
        assert!(hub.identify(&hdrs(Some("chris"), None)).await.is_some());

        hub.remove("stub");
        assert!(
            hub.identify(&hdrs(Some("chris"), None)).await.is_none(),
            "uninstalled provider must not answer"
        );
        assert!(hub.owners().is_empty());
    }

    #[tokio::test]
    async fn retain_prunes_only_dead_owners() {
        let hub = IdentityHub::new();
        hub.register("alive", Arc::new(HeaderStub));
        hub.register("dead", Arc::new(HeaderStub));

        let live: HashSet<String> = ["alive".to_string()].into_iter().collect();
        hub.retain(&live);
        assert_eq!(hub.owners(), vec!["alive".to_string()]);
    }

    #[tokio::test]
    async fn sole_provider_detection() {
        let hub = IdentityHub::new();
        assert!(!hub.is_sole_enabled_provider("auth"), "no providers: nothing is sole");

        hub.register("auth", Arc::new(HeaderStub));
        assert!(hub.is_sole_enabled_provider("auth"));
        assert!(!hub.is_sole_enabled_provider("other"), "unknown owner is never sole");

        // A second provider removes the lockout risk.
        hub.register("backup", Arc::new(HeaderStub));
        assert!(!hub.is_sole_enabled_provider("auth"));

        // Disabling the other one restores it.
        hub.set_enabled("backup", false);
        assert!(hub.is_sole_enabled_provider("auth"));

        // And a disabled provider is not a provider at all.
        hub.set_enabled("auth", false);
        assert!(!hub.is_sole_enabled_provider("auth"));
    }

    #[tokio::test]
    async fn register_replaces_by_owner_key() {
        // hot-reload semantics: second register under the same owner wins
        let hub = IdentityHub::new();
        hub.register("auth", Arc::new(HeaderStub));
        hub.register("auth", Arc::new(HeaderStub));
        assert_eq!(hub.owners().len(), 1);
    }
}
