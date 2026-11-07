use ip_mcp::{McpAuth, McpEndpoint, McpId, McpName, McpServer, McpToken};

use crate::audit::By;
use crate::error::{Entity, StorageError};
use crate::list::Page;
use crate::store::Backend;

/// The account every suite test registers a server as, through the endpoint that registers one.
pub(crate) fn by() -> By {
    By::new(crate::suite::fixture::user_id(), "/_ip/mcps")
}

/// A server under `id`, called `name`, requiring nothing.
pub(crate) fn server(id: &str, name: &str) -> McpServer {
    McpServer {
        id: McpId::new(id).unwrap(),
        name: McpName::new(name).unwrap(),
        endpoint: McpEndpoint::new("https://mcp.acme.test/sse").unwrap(),
        auth: McpAuth::None,
    }
}

/// The same, requiring that bearer token.
pub(crate) fn bearer(id: &str, name: &str, token: &str) -> McpServer {
    McpServer {
        auth: McpAuth::Bearer {
            token: McpToken::new(token).unwrap(),
        },
        ..server(id, name)
    }
}

/// The token of a server read back, and none where it requires nothing.
fn token_of(read: &crate::mcp::StoredMcp) -> Option<&str> {
    read.server.auth.token().map(McpToken::expose)
}

pub(crate) async fn a_server_round_trips(store: &impl Backend) {
    let written = store
        .put_mcp(server("warehouse", "Warehouse tools"), &by())
        .await
        .unwrap();

    assert_eq!(written.server, server("warehouse", "Warehouse tools"));

    let read = store
        .mcp(&McpId::new("warehouse").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read, written);
}

/// The credential is the one column nothing else reads back, so it is asserted by value.
pub(crate) async fn a_credential_survives_the_database(store: &impl Backend) {
    const TOKEN: &str = "wh-live-9f2c4a7b1e";
    store
        .put_mcp(bearer("warehouse", "Warehouse", TOKEN), &by())
        .await
        .unwrap();

    let read = store
        .mcp(&McpId::new("warehouse").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(token_of(&read), Some(TOKEN));
}

pub(crate) async fn registering_a_server_again_replaces_it(store: &impl Backend) {
    store
        .put_mcp(server("warehouse", "First"), &by())
        .await
        .unwrap();
    store
        .put_mcp(server("warehouse", "Second"), &by())
        .await
        .unwrap();

    let held = store.mcps(Page::new(10, 0, None)).await.unwrap();
    assert_eq!(held.len(), 1, "one id, one server");
    assert_eq!(held[0].server.name.as_str(), "Second");
}

/// What the replacement must not do: leave the credential of the server it replaced behind.
pub(crate) async fn a_server_that_stops_requiring_a_token_keeps_none(store: &impl Backend) {
    let id = McpId::new("warehouse").unwrap();
    store
        .put_mcp(
            bearer("warehouse", "Warehouse", "wh-live-9f2c4a7b1e"),
            &by(),
        )
        .await
        .unwrap();
    store
        .put_mcp(server("warehouse", "Warehouse"), &by())
        .await
        .unwrap();

    let read = store.mcp(&id).await.unwrap().unwrap();
    assert_eq!(read.server.auth, McpAuth::None);
    assert_eq!(token_of(&read), None, "a token outlived the server's need");
}

pub(crate) async fn rotating_a_credential_leaves_one_server_holding_the_new_one(
    store: &impl Backend,
) {
    store
        .put_mcp(bearer("warehouse", "Warehouse", "wh-live-first"), &by())
        .await
        .unwrap();
    store
        .put_mcp(bearer("warehouse", "Warehouse", "wh-live-second"), &by())
        .await
        .unwrap();

    let held = store.mcps(Page::new(10, 0, None)).await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(token_of(&held[0]), Some("wh-live-second"));
}

pub(crate) async fn removing_a_server_leaves_the_others(store: &impl Backend) {
    store
        .put_mcp(server("warehouse", "Warehouse"), &by())
        .await
        .unwrap();
    store
        .put_mcp(server("tickets", "Tickets"), &by())
        .await
        .unwrap();

    store
        .remove_mcp(&McpId::new("warehouse").unwrap(), &by())
        .await
        .unwrap();

    let held = store.mcps(Page::new(10, 0, None)).await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].server.id.as_str(), "tickets");
}

pub(crate) async fn removing_a_server_that_is_absent_reports_it_missing(store: &impl Backend) {
    let error = store
        .remove_mcp(&McpId::new("warehouse").unwrap(), &by())
        .await
        .unwrap_err();

    assert!(
        matches!(error, StorageError::NotFound { entity: Entity::Mcp, ref id } if id == "warehouse"),
        "{error:?}"
    );
}

pub(crate) async fn servers_come_newest_first_a_page_at_a_time(store: &impl Backend) {
    for id in ["first", "second", "third"] {
        store.put_mcp(server(id, "Named"), &by()).await.unwrap();
    }

    let newest = store.mcps(Page::new(2, 0, None)).await.unwrap();
    let rest = store.mcps(Page::new(2, 2, None)).await.unwrap();

    let named: Vec<&str> = newest
        .iter()
        .chain(rest.iter())
        .map(|held| held.server.id.as_str())
        .collect();
    assert_eq!(named, ["third", "second", "first"]);
    assert_eq!(newest.len(), 2);
    assert_eq!(rest.len(), 1);
}

/// Deleting a tenant takes its templates with it; a server is nobody's, so it stays.
pub(crate) async fn deleting_a_tenant_leaves_every_server_where_it_was(store: &impl Backend) {
    let (tenant, _) = crate::suite::fixture::tenant_with_user(store).await;
    store
        .put_mcp(server("warehouse", "Warehouse"), &by())
        .await
        .unwrap();

    store
        .delete_tenant(&tenant.id, &crate::suite::identity::tenants_by())
        .await
        .unwrap();

    let held = store.mcps(Page::new(10, 0, None)).await.unwrap();
    assert_eq!(held.len(), 1, "a server belongs to the deployment");
}
