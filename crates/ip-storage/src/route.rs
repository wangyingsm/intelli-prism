use async_trait::async_trait;
use ip_core::{RouteKey, RouteRule};

use crate::error::StorageError;

/// Reads and writes the routing rules held in the database.
///
/// These are the dynamic half of the routing table. The static half comes from the
/// configuration file and wins wherever the two name the same key.
#[async_trait]
pub trait RouteStore: Send + Sync {
    /// Records a rule, replacing whatever the key routed to before.
    async fn put_route(&self, rule: RouteRule) -> Result<(), StorageError>;

    /// Removes the rule under a key, or reports it missing.
    async fn remove_route(&self, key: &RouteKey) -> Result<(), StorageError>;

    /// Reads the rule under one key.
    async fn route(&self, key: &RouteKey) -> Result<Option<RouteRule>, StorageError>;

    /// Every rule held, for building the routing table.
    async fn routes(&self) -> Result<Vec<RouteRule>, StorageError>;
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ip_core::{AbsPath, Endpoint, Host, Port, Protocol, RouteTarget};

    use super::*;
    use crate::error::Entity;

    #[derive(Default)]
    struct MemoryRoutes(Mutex<Vec<RouteRule>>);

    #[async_trait]
    impl RouteStore for MemoryRoutes {
        async fn put_route(&self, rule: RouteRule) -> Result<(), StorageError> {
            let mut held = self.0.lock().unwrap();
            held.retain(|existing| existing.key != rule.key);
            held.push(rule);
            Ok(())
        }

        async fn remove_route(&self, key: &RouteKey) -> Result<(), StorageError> {
            let mut held = self.0.lock().unwrap();
            let before = held.len();
            held.retain(|existing| &existing.key != key);
            if held.len() == before {
                return Err(StorageError::NotFound {
                    entity: Entity::Route,
                    id: key.to_string(),
                });
            }
            Ok(())
        }

        async fn route(&self, key: &RouteKey) -> Result<Option<RouteRule>, StorageError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .find(|existing| &existing.key == key)
                .cloned())
        }

        async fn routes(&self) -> Result<Vec<RouteRule>, StorageError> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    fn endpoint(host: &str, path: &str) -> Endpoint {
        Endpoint {
            protocol: Protocol::Https,
            host: Host::new(host).unwrap(),
            port: Port::new(443).unwrap(),
            path: AbsPath::new(path).unwrap(),
        }
    }

    fn rule(host: &str, path: &str, upstream: &str) -> RouteRule {
        RouteRule {
            key: ip_core::RouteKey::new(endpoint(host, path)),
            target: RouteTarget::new(endpoint(upstream, path)),
        }
    }

    fn store() -> Arc<dyn RouteStore> {
        Arc::new(MemoryRoutes::default())
    }

    #[tokio::test]
    async fn the_surface_is_reachable_through_one_trait_object() {
        let store = store();
        let rule = rule("gateway.local", "/v1/messages", "api.example.com");
        store.put_route(rule.clone()).await.unwrap();
        assert_eq!(store.route(&rule.key).await.unwrap(), Some(rule.clone()));
        assert_eq!(store.routes().await.unwrap(), vec![rule.clone()]);
        store.remove_route(&rule.key).await.unwrap();
        assert_eq!(store.route(&rule.key).await.unwrap(), None);
    }

    #[tokio::test]
    async fn writing_the_same_key_replaces_its_target() {
        let store = store();
        store
            .put_route(rule("gateway.local", "/v1", "first.example.com"))
            .await
            .unwrap();
        let second = rule("gateway.local", "/v1", "second.example.com");
        store.put_route(second.clone()).await.unwrap();
        assert_eq!(store.routes().await.unwrap(), vec![second]);
    }

    #[tokio::test]
    async fn keys_differing_only_by_path_are_different_rules() {
        let store = store();
        store
            .put_route(rule("gateway.local", "/v1", "api.example.com"))
            .await
            .unwrap();
        store
            .put_route(rule("gateway.local", "/v2", "api.example.com"))
            .await
            .unwrap();
        assert_eq!(store.routes().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn removing_what_is_absent_reports_it_missing() {
        let store = store();
        let key = ip_core::RouteKey::new(endpoint("gateway.local", "/v1"));
        assert!(matches!(
            store.remove_route(&key).await,
            Err(StorageError::NotFound {
                entity: Entity::Route,
                ..
            })
        ));
    }
}
