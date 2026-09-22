use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use ip_core::{
    ApiId, Checksum, NewPluginRule, PluginKind, PluginOrder, PluginRule, PluginScope, Timestamp,
    UserId,
};
use ip_plugin::{PluginHost, PluginLimits};
use ip_storage::{Entity, Listed, NewPlugin, PluginOwner, PluginRecord, Standing, StorageError};

use super::{Paged, store_refusal};
use crate::manage::Manager;
use crate::state::AppState;

/// The plugins the caller's chain owns, under the prefix the router nests them at.
///
/// Who calls decides whose plugins these are: the system administrator's are the global
/// chain's, and a tenant owner's are its tenant's. Wasm is stored once however many own it,
/// but each owner sees, places and lets go of only its own.
pub fn store_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_plugins).post(upload))
        .route("/{checksum}", delete(disown))
}

/// The chain the caller manages, under the prefix the router nests it at.
///
/// Who calls decides which chain: the system administrator manages the global chain and
/// only that, and a tenant's owner manages its own tenant's chain. Nobody else places
/// plugins.
pub fn rules_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_rules).post(place_rule))
        .route("/{kind}/{order}", delete(remove_rule))
}

/// A plugin as its owner sees it.
#[derive(Debug, serde::Serialize)]
pub struct PluginView {
    /// The sha256 of its wasm, in hex, which is its name.
    pub checksum: String,
    /// Where in the dataflow it runs.
    pub kind: &'static str,
    /// How large its wasm is, in bytes.
    pub size: usize,
    /// When this owner stored it, in seconds since the unix epoch.
    pub created_at: Timestamp,
}

/// A plugin placed in a chain, as the management api shows it.
#[derive(Debug, serde::Serialize)]
pub struct RuleView {
    /// The plugin, by checksum.
    pub checksum: String,
    /// The chain it joins.
    pub kind: &'static str,
    /// Where it sits in that chain; higher runs first.
    pub order: u8,
    /// The one user it applies to, when it is narrowed to one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The one api it applies to, when it is narrowed to one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// When it was placed, in seconds since the unix epoch, as a list reads it back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<Timestamp>,
}

/// A response turned away early, boxed so it passes back up a `Result` cheaply.
type Refusal = Box<Response<Body>>;

/// Which chain an uploaded plugin is for.
#[derive(Debug, serde::Deserialize)]
pub struct UploadQuery {
    /// The plugin's kind, as `req_body` and the rest spell it.
    kind: Option<String>,
}

/// Where a plugin is placed in the caller's chain.
#[derive(Debug, serde::Deserialize)]
pub struct Placement {
    /// The plugin, by checksum.
    checksum: String,
    /// Where it sits: 0 to 63 in the global chain, above that in a tenant's.
    order: u8,
    /// Narrows a tenant's rule to one member.
    user: Option<String>,
    /// Narrows a tenant's rule to one api.
    api: Option<String>,
}

/// The plugins the caller's chain owns, newest first.
async fn list_plugins(
    State(state): State<AppState>,
    manager: Manager,
    Paged(page): Paged,
) -> Response<Body> {
    let owner = match owner_of(&state, &manager).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    match state.stores().backend().list_plugins(&owner, page).await {
        Ok(listed) => {
            Json(listed.into_iter().map(PluginView::from).collect::<Vec<_>>()).into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// Stores wasm for one kind of chain on the caller's behalf, once it has compiled and shown
/// the plugin abi.
///
/// Answers 201 with the plugin's checksum, which is what a rule names it by. Wasm the caller
/// stored already is answered the same way; wasm stored as another kind is refused.
async fn upload(
    State(state): State<AppState>,
    manager: Manager,
    Query(query): Query<UploadQuery>,
    wasm: Bytes,
) -> Response<Body> {
    let owner = match owner_of(&state, &manager).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    let Some(Ok(kind)) = query.kind.as_deref().map(str::parse::<PluginKind>) else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "name the plugin's kind as ?kind=",
        )
            .into_response();
    };
    let wasm = wasm.to_vec();
    if let Some(refusal) = compile_refusal(wasm.clone()).await {
        return refusal;
    }
    let backend = state.stores().backend();
    let stored = backend.put_plugin(NewPlugin { kind, wasm, owner }).await;
    match stored {
        Ok(record) => (StatusCode::CREATED, Json(PluginView::from(record))).into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Lets go of one of the caller's plugins, refused while its chain still runs it. The wasm
/// goes once nobody owns it.
async fn disown(
    State(state): State<AppState>,
    manager: Manager,
    Path(checksum): Path<String>,
) -> Response<Body> {
    let owner = match owner_of(&state, &manager).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    let Ok(checksum) = Checksum::from_hex(&checksum) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match state
        .stores()
        .backend()
        .disown_plugin(&checksum, &owner)
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Every rule in the caller's chain, newest first.
async fn list_rules(
    State(state): State<AppState>,
    manager: Manager,
    Paged(page): Paged,
) -> Response<Body> {
    let owner = match owner_of(&state, &manager).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    match state
        .stores()
        .backend()
        .list_rules(owner.tenant(), page)
        .await
    {
        Ok(listed) => {
            Json(listed.into_iter().map(RuleView::from).collect::<Vec<_>>()).into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// Places one of the caller's plugins in the caller's chain.
async fn place_rule(
    State(state): State<AppState>,
    manager: Manager,
    body: Bytes,
) -> Response<Body> {
    let owner = match owner_of(&state, &manager).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    let Json(placement) = match Json::<Placement>::from_bytes(&body) {
        Ok(placement) => placement,
        Err(rejection) => return rejection.into_response(),
    };
    let scope = match owner {
        PluginOwner::Global if placement.user.is_some() || placement.api.is_some() => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                "a global rule runs for every flow, so it names no user and no api",
            )
                .into_response();
        }
        PluginOwner::Global => PluginScope::Global,
        PluginOwner::Tenant(tenant) => {
            let (user, api) = match (
                placement.user.as_deref().map(UserId::new).transpose(),
                placement.api.as_deref().map(ApiId::new).transpose(),
            ) {
                (Ok(user), Ok(api)) => (user, api),
                _ => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
            };
            if let Some(user) = &user {
                match state.store().membership(user, &tenant).await {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        return (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            format!("{user} is not in {tenant}"),
                        )
                            .into_response();
                    }
                    Err(error) => return store_refusal(error),
                }
            }
            PluginScope::Tenant { tenant, user, api }
        }
    };
    let Ok(checksum) = Checksum::from_hex(&placement.checksum) else {
        return unknown_plugin();
    };
    let rule = match NewPluginRule::new(checksum, PluginOrder::new(placement.order), scope) {
        Ok(rule) => rule,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response(),
    };
    match state.stores().backend().put_rule(rule).await {
        Ok(placed) => {
            state.feed().after_change().await;
            (StatusCode::CREATED, Json(RuleView::from(&placed))).into_response()
        }
        Err(StorageError::NotFound {
            entity: Entity::Plugin,
            ..
        }) => unknown_plugin(),
        Err(error) => store_refusal(error),
    }
}

/// Removes the rule at one kind and order from the caller's chain.
async fn remove_rule(
    State(state): State<AppState>,
    manager: Manager,
    Path((kind, order)): Path<(String, u8)>,
) -> Response<Body> {
    let owner = match owner_of(&state, &manager).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    let Ok(kind) = kind.parse::<PluginKind>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match state
        .stores()
        .backend()
        .remove_rule(owner.tenant(), kind, PluginOrder::new(order))
        .await
    {
        Ok(()) => {
            state.feed().after_change().await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// A plugin the caller's chain does not own reads as one that is not there, so no chain
/// learns what another has stored.
fn unknown_plugin() -> Response<Body> {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        "your chain holds no plugin under that checksum",
    )
        .into_response()
}

/// Whose plugins the caller manages, or what it is told when it manages none.
async fn owner_of(state: &AppState, manager: &Manager) -> Result<PluginOwner, Refusal> {
    if manager.is_system_administrator() {
        return Ok(PluginOwner::Global);
    }
    let owned = match state.store().memberships_of_user(manager.user()).await {
        Ok(memberships) => memberships
            .into_iter()
            .find(|held| held.standing == Standing::Owner),
        Err(error) => return Err(Box::new(store_refusal(error))),
    };
    // An owner is created with its tenant and owns no other, so the first is the only one.
    match owned {
        Some(held) => Ok(PluginOwner::Tenant(held.tenant)),
        None => Err(Box::new(StatusCode::FORBIDDEN.into_response())),
    }
}

/// Compiles wasm the way the live host will, in a host of its own so nothing the gateway
/// runs is touched, and answers what the uploader is told when it does not load.
async fn compile_refusal(wasm: Vec<u8>) -> Option<Response<Body>> {
    let checked = tokio::task::spawn_blocking(move || {
        let host = PluginHost::on_demand(PluginLimits::default())?;
        host.load(&Checksum::of(&wasm), &wasm)
    })
    .await;
    match checked {
        Ok(Ok(())) => None,
        Ok(Err(error)) => {
            Some((StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response())
        }
        Err(error) => {
            tracing::error!(%error, "a plugin check did not finish");
            Some(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

impl From<Listed<PluginRecord>> for PluginView {
    fn from(listed: Listed<PluginRecord>) -> Self {
        Self {
            created_at: listed.created_at,
            ..Self::from(listed.item)
        }
    }
}

impl From<PluginRecord> for PluginView {
    fn from(record: PluginRecord) -> Self {
        Self {
            checksum: record.checksum.to_hex(),
            kind: record.kind.name(),
            size: record.size,
            created_at: record.created_at,
        }
    }
}

impl From<Listed<PluginRule>> for RuleView {
    fn from(listed: Listed<PluginRule>) -> Self {
        Self {
            created_at: Some(listed.created_at),
            ..Self::from(&listed.item)
        }
    }
}

impl From<&PluginRule> for RuleView {
    fn from(rule: &PluginRule) -> Self {
        let (user, api) = match rule.scope() {
            PluginScope::Global => (None, None),
            PluginScope::Tenant { user, api, .. } => (
                user.as_ref().map(ToString::to_string),
                api.as_ref().map(ToString::to_string),
            ),
        };
        Self {
            checksum: rule.checksum().to_hex(),
            kind: rule.kind().name(),
            order: rule.order().get(),
            user,
            api,
            created_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use ip_core::{ApiId, Capability, CapabilityScope, Grant, TenantId, TnKey};
    use ip_storage::{
        AccountKind, GrantStore, Membership, MembershipStore, NewTenant, NewUser, SqliteStore,
        TenantStore, UserStore,
    };
    use tower::ServiceExt;

    use super::super::harness::{body_of, cookie_of, hashed, request, state_over};
    use super::*;
    use crate::manage::CSRF_HEADER;

    /// A plugin that loads, whose bytes differ with `tag` so each tag is its own plugin.
    fn wasm(tag: &str) -> Vec<u8> {
        wat::parse_str(format!(
            r#"(module
  (memory (export "memory") 1)
  (data (i32.const 16) "{tag}")
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "dealloc") (param i32 i32))
  (func (export "transform") (param i32 i32) (result i64)
    (i64.or (i64.shl (i64.const 16) (i64.const 32)) (i64.const 1))))"#
        ))
        .unwrap()
    }

    fn id(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    fn acme() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    /// The server's own router over acme, owned by alice, with bob holding `ApiAdvMgr` on the
    /// chat api, which places no plugins, and carol holding nothing; and globex, owned by
    /// dave. Root is the system administrator.
    async fn fixture() -> (Router, AppState) {
        let store = SqliteStore::in_memory().await.unwrap();
        for tenant in [acme(), TenantId::new("globex").unwrap()] {
            store
                .create_tenant(NewTenant {
                    id: tenant,
                    key: TnKey::generate().unwrap(),
                })
                .await
                .unwrap();
        }
        for (user, kind) in [
            ("root", AccountKind::SystemAdministrator),
            ("alice", AccountKind::Regular),
            ("bob", AccountKind::Regular),
            ("carol", AccountKind::Regular),
            ("dave", AccountKind::Regular),
        ] {
            store
                .create_user(NewUser {
                    id: id(user),
                    passphrase: hashed(),
                    kind,
                })
                .await
                .unwrap();
        }
        for (user, tenant, standing) in [
            ("alice", "acme", Standing::Owner),
            ("bob", "acme", Standing::Member),
            ("carol", "acme", Standing::Member),
            ("dave", "globex", Standing::Owner),
        ] {
            store
                .attach(Membership {
                    user: id(user),
                    tenant: TenantId::new(tenant).unwrap(),
                    standing,
                })
                .await
                .unwrap();
        }
        for capability in [Capability::ApiAccess, Capability::ApiAdvMgr] {
            let scope = CapabilityScope::Api {
                user: id("bob"),
                tenant: acme(),
                api: ApiId::new("chat").unwrap(),
            };
            store
                .grant(&Grant::new(capability, scope).unwrap())
                .await
                .unwrap();
        }
        let state = state_over(store);
        (crate::routes::router(state.clone()), state)
    }

    async fn upload(
        router: &Router,
        state: &AppState,
        caller: &str,
        kind: &str,
        wasm: Vec<u8>,
    ) -> axum::http::Response<Body> {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/_ip/plugins?kind={kind}"))
            .header("cookie", cookie_of(state, &id(caller)).await)
            .header(CSRF_HEADER, "1")
            .body(Body::from(wasm))
            .unwrap();
        router.clone().oneshot(request).await.unwrap()
    }

    async fn call(
        router: &Router,
        state: &AppState,
        caller: &str,
        method: &str,
        uri: &str,
        body: Option<&str>,
    ) -> axum::http::Response<Body> {
        router
            .clone()
            .oneshot(request(
                method,
                uri,
                &cookie_of(state, &id(caller)).await,
                body,
            ))
            .await
            .unwrap()
    }

    async fn json(response: axum::http::Response<Body>) -> serde_json::Value {
        serde_json::from_str(&body_of(response).await).unwrap()
    }

    /// Uploads a plugin for `caller`'s chain and hands back its checksum.
    async fn stored(router: &Router, state: &AppState, caller: &str, tag: &str) -> String {
        let response = upload(router, state, caller, "resp_body", wasm(tag)).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        json(response).await["checksum"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn placing(checksum: &str, order: u8) -> String {
        serde_json::json!({"checksum": checksum, "order": order}).to_string()
    }

    const RULES: &str = "/_ip/plugin-rules";

    #[tokio::test]
    async fn an_owner_uploads_a_plugin_named_by_its_own_checksum() {
        let (router, state) = fixture().await;
        let response = upload(&router, &state, "alice", "resp_body", wasm("one")).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = json(response).await;
        assert_eq!(body["checksum"], Checksum::of(&wasm("one")).to_hex());
        assert_eq!(body["kind"], "resp_body");
        assert!(body["created_at"].is_i64());

        let listed = json(call(&router, &state, "alice", "GET", "/_ip/plugins", None).await).await;
        assert_eq!(listed.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_same_wasm_is_stored_once_and_for_one_kind() {
        let (router, state) = fixture().await;
        for _ in 0..2 {
            let again = upload(&router, &state, "alice", "resp_body", wasm("one")).await;
            assert_eq!(again.status(), StatusCode::CREATED);
        }
        let other_kind = upload(&router, &state, "alice", "req_body", wasm("one")).await;
        assert_eq!(other_kind.status(), StatusCode::CONFLICT);
        assert_eq!(state.stores().backend().plugins().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn wasm_the_host_would_not_load_is_not_stored() {
        let (router, state) = fixture().await;
        let importing = wat::parse_str(r#"(module (import "env" "clock" (func)))"#).unwrap();
        for wasm in [b"not wasm at all".to_vec(), importing, Vec::new()] {
            let response = upload(&router, &state, "alice", "resp_body", wasm).await;
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        }
        for kind in ["", "resp_everything"] {
            let response = upload(&router, &state, "alice", kind, wasm("one")).await;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{kind}"
            );
        }
        assert!(state.stores().backend().plugins().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_the_administrator_and_owners_touch_plugins() {
        let (router, state) = fixture().await;
        for caller in ["bob", "carol"] {
            let upload = upload(&router, &state, caller, "resp_body", wasm("one")).await;
            assert_eq!(upload.status(), StatusCode::FORBIDDEN, "{caller}");
            for (method, uri, body) in [
                ("GET", "/_ip/plugins", None),
                ("GET", RULES, None),
                ("POST", RULES, Some("{}")),
                ("DELETE", "/_ip/plugin-rules/resp_body/100", None),
            ] {
                let response = call(&router, &state, caller, method, uri, body).await;
                assert_eq!(
                    response.status(),
                    StatusCode::FORBIDDEN,
                    "{caller} {method} {uri}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_plugin_goes_only_once_no_rule_runs_it() {
        let (router, state) = fixture().await;
        let checksum = stored(&router, &state, "root", "one").await;
        let placed = call(
            &router,
            &state,
            "root",
            "POST",
            RULES,
            Some(&placing(&checksum, 10)),
        )
        .await;
        assert_eq!(placed.status(), StatusCode::CREATED);

        let path = format!("/_ip/plugins/{checksum}");
        let not_hers = call(&router, &state, "alice", "DELETE", &path, None).await;
        assert_eq!(not_hers.status(), StatusCode::NOT_FOUND);
        let in_use = call(&router, &state, "root", "DELETE", &path, None).await;
        assert_eq!(in_use.status(), StatusCode::CONFLICT);

        call(
            &router,
            &state,
            "root",
            "DELETE",
            "/_ip/plugin-rules/resp_body/10",
            None,
        )
        .await;
        let removed = call(&router, &state, "root", "DELETE", &path, None).await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        assert!(state.stores().backend().plugins().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_administrator_places_in_the_global_chain_at_primary_orders() {
        let (router, state) = fixture().await;
        let checksum = stored(&router, &state, "root", "one").await;

        let placed = call(
            &router,
            &state,
            "root",
            "POST",
            RULES,
            Some(&placing(&checksum, 10)),
        )
        .await;
        assert_eq!(placed.status(), StatusCode::CREATED);
        let placed = json(placed).await;
        assert_eq!(placed["kind"], "resp_body");
        assert_eq!(placed["order"], 10);

        let tenant_order = call(
            &router,
            &state,
            "root",
            "POST",
            RULES,
            Some(&placing(&checksum, 100)),
        )
        .await;
        assert_eq!(tenant_order.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let narrowed =
            serde_json::json!({"checksum": checksum, "order": 11, "api": "chat"}).to_string();
        let narrowed = call(&router, &state, "root", "POST", RULES, Some(&narrowed)).await;
        assert_eq!(narrowed.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let unknown = placing(&Checksum::of(b"never stored").to_hex(), 12);
        let missing = call(&router, &state, "root", "POST", RULES, Some(&unknown)).await;
        assert_eq!(missing.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn an_owner_places_in_its_tenant_s_chain_above_the_primary_orders() {
        let (router, state) = fixture().await;
        let checksum = stored(&router, &state, "alice", "one").await;

        let placed = call(
            &router,
            &state,
            "alice",
            "POST",
            RULES,
            Some(&placing(&checksum, 100)),
        )
        .await;
        assert_eq!(placed.status(), StatusCode::CREATED);
        let rules = state
            .stores()
            .backend()
            .rules_for_tenant(&acme())
            .await
            .unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].scope().tenant(), Some(&acme()));

        let taken = call(
            &router,
            &state,
            "alice",
            "POST",
            RULES,
            Some(&placing(&checksum, 100)),
        )
        .await;
        assert_eq!(taken.status(), StatusCode::CONFLICT);

        let primary = call(
            &router,
            &state,
            "alice",
            "POST",
            RULES,
            Some(&placing(&checksum, 10)),
        )
        .await;
        assert_eq!(primary.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let narrowed =
            serde_json::json!({"checksum": checksum, "order": 101, "user": "bob", "api": "chat"})
                .to_string();
        let narrowed = call(&router, &state, "alice", "POST", RULES, Some(&narrowed)).await;
        assert_eq!(narrowed.status(), StatusCode::CREATED);
        let narrowed = json(narrowed).await;
        assert_eq!(narrowed["user"], "bob");
        assert_eq!(narrowed["api"], "chat");
    }

    #[tokio::test]
    async fn each_caller_reads_only_the_chain_it_manages() {
        let (router, state) = fixture().await;
        let checksum = stored(&router, &state, "root", "one").await;
        stored(&router, &state, "alice", "one").await;
        call(
            &router,
            &state,
            "root",
            "POST",
            RULES,
            Some(&placing(&checksum, 10)),
        )
        .await;
        call(
            &router,
            &state,
            "alice",
            "POST",
            RULES,
            Some(&placing(&checksum, 100)),
        )
        .await;

        for (caller, order) in [("root", 10), ("alice", 100)] {
            let listed = json(call(&router, &state, caller, "GET", RULES, None).await).await;
            assert_eq!(listed.as_array().unwrap().len(), 1, "{caller}");
            assert_eq!(listed[0]["order"], order, "{caller}");
        }
    }

    #[tokio::test]
    async fn a_rule_narrowed_to_someone_outside_the_tenant_is_refused() {
        let (router, state) = fixture().await;
        let checksum = stored(&router, &state, "alice", "one").await;
        let body =
            serde_json::json!({"checksum": checksum, "order": 100, "user": "root"}).to_string();
        let response = call(&router, &state, "alice", "POST", RULES, Some(&body)).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_rule_is_removed_only_from_the_chain_the_caller_manages() {
        let (router, state) = fixture().await;
        let checksum = stored(&router, &state, "alice", "one").await;
        call(
            &router,
            &state,
            "alice",
            "POST",
            RULES,
            Some(&placing(&checksum, 100)),
        )
        .await;
        let path = "/_ip/plugin-rules/resp_body/100";

        let not_global = call(&router, &state, "root", "DELETE", path, None).await;
        assert_eq!(not_global.status(), StatusCode::NOT_FOUND);

        let removed = call(&router, &state, "alice", "DELETE", path, None).await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        assert!(
            state
                .stores()
                .backend()
                .rules_for_tenant(&acme())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn each_owner_sees_and_places_only_its_own_plugins() {
        let (router, state) = fixture().await;
        let alices = stored(&router, &state, "alice", "alice's").await;
        let roots = stored(&router, &state, "root", "root's").await;

        for (caller, own) in [("alice", &alices), ("root", &roots)] {
            let listed =
                json(call(&router, &state, caller, "GET", "/_ip/plugins", None).await).await;
            assert_eq!(listed.as_array().unwrap().len(), 1, "{caller}");
            assert_eq!(&listed[0]["checksum"], own.as_str(), "{caller}");
        }
        let daves = json(call(&router, &state, "dave", "GET", "/_ip/plugins", None).await).await;
        assert!(daves.as_array().unwrap().is_empty());

        let not_hers = call(
            &router,
            &state,
            "alice",
            "POST",
            RULES,
            Some(&placing(&roots, 100)),
        )
        .await;
        assert_eq!(not_hers.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let not_his = call(
            &router,
            &state,
            "dave",
            "POST",
            RULES,
            Some(&placing(&alices, 100)),
        )
        .await;
        assert_eq!(not_his.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn shared_wasm_stays_until_its_last_owner_lets_it_go() {
        let (router, state) = fixture().await;
        let checksum = stored(&router, &state, "alice", "shared").await;
        stored(&router, &state, "dave", "shared").await;
        let path = format!("/_ip/plugins/{checksum}");

        let alice_lets_go = call(&router, &state, "alice", "DELETE", &path, None).await;
        assert_eq!(alice_lets_go.status(), StatusCode::NO_CONTENT);
        let checksum = Checksum::from_hex(&checksum).unwrap();
        assert!(
            state
                .stores()
                .backend()
                .plugin(&checksum)
                .await
                .unwrap()
                .is_some()
        );
        let alices = json(call(&router, &state, "alice", "GET", "/_ip/plugins", None).await).await;
        assert!(alices.as_array().unwrap().is_empty());

        let dave_lets_go = call(&router, &state, "dave", "DELETE", &path, None).await;
        assert_eq!(dave_lets_go.status(), StatusCode::NO_CONTENT);
        assert!(
            state
                .stores()
                .backend()
                .plugin(&checksum)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn plugins_and_rules_are_listed_newest_first_a_page_at_a_time() {
        let (router, state) = fixture().await;
        let mut checksums = Vec::new();
        for tag in ["first", "second", "third"] {
            checksums.push(stored(&router, &state, "alice", tag).await);
        }
        for (order, checksum) in [100, 101, 102].into_iter().zip(&checksums) {
            call(
                &router,
                &state,
                "alice",
                "POST",
                RULES,
                Some(&placing(checksum, order)),
            )
            .await;
        }

        let page = json(
            call(
                &router,
                &state,
                "alice",
                "GET",
                "/_ip/plugins?limit=2&offset=1",
                None,
            )
            .await,
        )
        .await;
        let listed: Vec<&str> = page
            .as_array()
            .unwrap()
            .iter()
            .map(|plugin| plugin["checksum"].as_str().unwrap())
            .collect();
        assert_eq!(listed, [checksums[1].as_str(), checksums[0].as_str()]);

        let rules = json(
            call(
                &router,
                &state,
                "alice",
                "GET",
                "/_ip/plugin-rules?limit=1",
                None,
            )
            .await,
        )
        .await;
        assert_eq!(rules.as_array().unwrap().len(), 1);
        assert_eq!(rules[0]["order"], 102);
        assert!(rules[0]["created_at"].is_i64());

        let later = json(
            call(
                &router,
                &state,
                "alice",
                "GET",
                "/_ip/plugins?after=4102444800",
                None,
            )
            .await,
        )
        .await;
        assert!(later.as_array().unwrap().is_empty());
    }
}
