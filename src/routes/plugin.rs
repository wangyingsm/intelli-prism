use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use ip_core::{Checksum, PluginKind, Timestamp};
use ip_plugin::{PluginHost, PluginLimits};
use ip_storage::{Listed, NewPlugin, PluginOwner, PluginRecord, Standing};

use super::{Paged, store_refusal};
use crate::manage::Manager;
use crate::state::AppState;

/// The plugins the caller's chain owns, under the prefix the router nests them at.
///
/// Who calls decides whose plugins these are: the system administrator's are the global
/// chain's, and a tenant owner's are its tenant's. Wasm is stored once however many own it,
/// but each owner sees and lets go of only its own.
pub fn store_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_plugins).post(upload))
        .route("/{checksum}", delete(disown))
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

/// A response turned away early, boxed so it passes back up a `Result` cheaply.
type Refusal = Box<Response<Body>>;

/// Which chain an uploaded plugin is for.
#[derive(Debug, serde::Deserialize)]
pub struct UploadQuery {
    /// The plugin's kind, as `req_body` and the rest spell it.
    kind: Option<String>,
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

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use ip_core::{ApiId, Capability, CapabilityScope, Grant, TenantId, TnKey, UserId};
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
            for (method, uri, body) in [("GET", "/_ip/plugins", None)] {
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
    async fn each_owner_sees_only_its_own_plugins() {
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
    async fn plugins_are_listed_newest_first_a_page_at_a_time() {
        let (router, state) = fixture().await;
        let mut checksums = Vec::new();
        for tag in ["first", "second", "third"] {
            checksums.push(stored(&router, &state, "alice", tag).await);
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
