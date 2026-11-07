//! MCP servers as they are stored, and what the trail says of one.

use async_trait::async_trait;
use ip_core::{TenantId, Timestamp};
use ip_mcp::{McpId, McpServer};

use crate::audit::By;
use crate::error::StorageError;
use crate::list::Page;

/// A server as it is stored.
///
/// There is no owner beside it, unlike an agent template: an MCP server is the deployment's, and
/// one id names one server for everybody.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMcp {
    /// What it is.
    pub server: McpServer,
    /// When it was written.
    pub created_at: Timestamp,
}

impl crate::audit::Auditable for StoredMcp {
    fn entity(&self) -> crate::error::Entity {
        crate::error::Entity::Mcp
    }

    fn entity_id(&self) -> String {
        self.server.id.as_str().to_owned()
    }

    /// A server is the deployment's, so it belongs to no tenant and the row says none.
    fn tenant(&self) -> Option<TenantId> {
        None
    }

    /// What the server is, and of its credential only which kind it is.
    ///
    /// The token is left out, and so is any digest of it. A digest would be a credential the api
    /// never hands back, written into a table kept for years and read by accounts that are shown
    /// everything in it — which is an offline guess at somebody's warehouse rather than a record
    /// of what changed.
    ///
    /// The cost is that one token replaced by another reads as a write whose two sides are alike.
    /// That the credential was replaced is what such a row says; what it was replaced with is not
    /// recoverable from the trail, and nor is what it was before.
    fn audited(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.server.name.as_str(),
            "endpoint": self.server.endpoint.as_str(),
            "auth": self.server.auth.name(),
        })
    }
}

/// Writes and reads the MCP servers an operator has registered.
///
/// Deployment-wide, so no owner is part of a lookup here. Nothing holds these in memory and no
/// node is told to reload them, unlike a routing rule, a limit or a price: whatever starts a run
/// reads the server a template names when it needs to reach it.
#[async_trait]
pub trait McpStore: Send + Sync {
    /// Writes a server, replacing whatever was stored under the same id.
    async fn put_mcp(&self, server: McpServer, by: &By) -> Result<StoredMcp, StorageError>;

    /// Removes a server, or reports it missing.
    ///
    /// A template naming it is left as it is: a tool that cannot be reached is refused when a run
    /// resolves it, not when the server it came from goes away.
    async fn remove_mcp(&self, id: &McpId, by: &By) -> Result<(), StorageError>;

    /// One server, or nothing where none is stored under the id.
    async fn mcp(&self, id: &McpId) -> Result<Option<StoredMcp>, StorageError>;

    /// The servers registered, newest first.
    async fn mcps(&self, page: Page) -> Result<Vec<StoredMcp>, StorageError>;
}

#[cfg(test)]
mod tests {
    use ip_core::UserId;
    use ip_mcp::{McpAuth, McpEndpoint, McpName, McpToken};

    use super::*;
    use crate::audit::{Auditable, NewAudit};
    use crate::error::Entity;

    const TOKEN: &str = "wh-live-9f2c4a7b1e";

    fn server(auth: McpAuth) -> McpServer {
        McpServer {
            id: McpId::new("warehouse").unwrap(),
            name: McpName::new("Warehouse tools").unwrap(),
            endpoint: McpEndpoint::new("https://mcp.acme.test/sse").unwrap(),
            auth,
        }
    }

    fn bearer(token: &str) -> McpAuth {
        McpAuth::Bearer {
            token: McpToken::new(token).unwrap(),
        }
    }

    fn stored(auth: McpAuth) -> StoredMcp {
        StoredMcp {
            server: server(auth),
            created_at: Timestamp::from_unix_seconds(1_700_000_000).unwrap(),
        }
    }

    #[test]
    fn a_recorded_server_names_itself_by_the_id_an_operator_spelled() {
        let mcp = stored(McpAuth::None);
        assert_eq!(mcp.entity(), Entity::Mcp);
        assert_eq!(mcp.entity_id(), "warehouse");
    }

    #[test]
    fn a_server_is_the_deployments_so_it_is_recorded_against_no_tenant() {
        assert_eq!(stored(McpAuth::None).tenant(), None);
        assert_eq!(stored(bearer(TOKEN)).tenant(), None);
    }

    #[test]
    fn the_trail_carries_what_a_server_is() {
        let recorded = stored(McpAuth::None).audited();
        assert_eq!(recorded["name"], "Warehouse tools");
        assert_eq!(recorded["endpoint"], "https://mcp.acme.test/sse");
        assert_eq!(recorded["auth"], "none");
    }

    #[test]
    fn the_trail_says_a_token_is_required_and_not_what_it_is() {
        let recorded = stored(bearer(TOKEN)).audited();
        assert_eq!(recorded["auth"], "bearer");
        let whole = serde_json::to_string(&recorded).unwrap();
        assert!(!whole.contains(TOKEN), "{whole}");
        assert!(!whole.contains("token"), "{whole}");
    }

    #[test]
    fn one_token_replaced_by_another_leaves_two_rows_that_read_alike() {
        assert_eq!(
            stored(bearer(TOKEN)).audited(),
            stored(bearer("wh-live-rotated-0001")).audited()
        );
    }

    #[test]
    fn a_recorded_change_carries_the_server_on_each_side_it_has_one() {
        let by = By::new(UserId::new("root").unwrap(), "/_ip/mcps");
        let mcp = stored(bearer(TOKEN));

        let created = NewAudit::created(by.clone(), &mcp);
        assert_eq!(created.entity, Entity::Mcp);
        assert_eq!(created.entity_id, "warehouse");
        assert_eq!(created.tenant, None);
        assert_eq!(created.before, None);
        assert_eq!(created.after, Some(mcp.audited()));

        let removed = NewAudit::removed(by, &mcp);
        assert_eq!(removed.before, Some(mcp.audited()));
        assert_eq!(removed.after, None);
    }
}
