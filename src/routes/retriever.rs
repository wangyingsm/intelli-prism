use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ip_core::{Capability, CapabilityScope, TenantId, Timestamp};
use ip_retrieve::{
    MaxHits, MetaSchema, Retriever, RetrieverAuth, RetrieverEndpoint, RetrieverId, RetrieverName,
    RetrieverParts, RetrieverTool, RetrieverTools, Source,
};
use ip_storage::{By, NewRetriever, RetrieverOwner, StoredRetriever};

use super::{Paged, store_refusal};
use crate::error::Unanswered;
use crate::manage::Manager;
use crate::retrieve::Backend;
use crate::state::AppState;

/// A refusal, boxed so the ordinary answer is not paid for by every caller.
type Refusal = Box<Response<Body>>;

/// The retrievers a tenant has described, under the prefix the router nests it at.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/{retriever}", get(read).put(put).delete(remove))
}

/// What the audit trail records a retriever written or removed as having come through.
const ENDPOINT: &str = "/_ip/retrievers";

/// One retriever, as the api reports it.
///
/// **There is no field a credential could travel in.** The key is not reported, not digested and
/// not replaced by a placeholder that something downstream might take for one: a caller that
/// wrote a key learns only that the kind of this retriever's credential is `key`. An operator
/// who has lost a key writes a new one.
#[derive(Debug, serde::Serialize)]
pub struct RetrieverView {
    /// Which retriever this is.
    pub id: String,
    /// What it is called.
    pub name: String,
    /// Whose it is, absent where it belongs to the deployment rather than to a tenant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Where its backend answers.
    pub endpoint: String,
    /// Which kind of credential it holds, and never the credential.
    pub auth: &'static str,
    /// Where it looks, and so what kind of retriever it is.
    pub source: Source,
    /// Which tools it offers.
    pub tools: RetrieverTools,
    /// What a look-up may filter on.
    pub meta: MetaSchema,
    /// The most hits one look-up answers with.
    pub max_hits: MaxHits,
    /// When it was written, in seconds since the unix epoch.
    pub created_at: Timestamp,
    /// What its backend said about its vectors, on the answer to the write that asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured: Option<Measured>,
}

/// What a retriever's backend said about its vectors when it was registered.
///
/// **On the answer to a write and on no other answer**, because the write is the one moment the
/// backend is asked: the width is not stored, so a later read has nothing of its own to report and
/// says nothing rather than repeating what was true once.
#[derive(Debug, serde::Serialize)]
pub struct Measured {
    /// How wide they are, and absent where the backend held none to measure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// What that means for this retriever, in words an operator reads.
    pub note: &'static str,
}

impl From<StoredRetriever> for RetrieverView {
    fn from(stored: StoredRetriever) -> Self {
        Self {
            id: stored.retriever.id().to_string(),
            name: stored.retriever.name().to_string(),
            tenant: stored.owner.tenant().map(ToString::to_string),
            endpoint: stored.retriever.endpoint().to_string(),
            auth: stored.retriever.auth().name(),
            source: stored.retriever.source().clone(),
            tools: stored.retriever.tools().clone(),
            meta: stored.retriever.meta().clone(),
            max_hits: stored.retriever.max_hits(),
            created_at: stored.created_at,
            measured: None,
        }
    }
}

/// A retriever as the api takes it. Which one it is comes from the path, not from here.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Written {
    /// What it is called.
    name: RetrieverName,
    /// Where its backend answers.
    endpoint: RetrieverEndpoint,
    /// What that backend requires; nothing when left out.
    #[serde(default)]
    auth: RetrieverAuth,
    /// Where it looks.
    source: Source,
    /// Which tools it offers.
    tools: RetrieverTools,
    /// What a look-up may filter on, and nothing when it may not.
    #[serde(default)]
    meta: MetaSchema,
    /// The most hits one look-up answers with.
    #[serde(default)]
    max_hits: MaxHits,
}

/// Whose retrievers a request is about.
#[derive(Debug, Default, serde::Deserialize)]
pub struct Owned {
    /// The tenant whose retrievers these are; the deployment's own when left out.
    tenant: Option<String>,
}

/// One owner's retrievers, newest first.
async fn list(
    State(state): State<AppState>,
    manager: Manager,
    Query(owned): Query<Owned>,
    Paged(page): Paged,
) -> Response<Body> {
    let owner = match owner_of(&state, &manager, owned).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    match state.store().retrievers(&owner, page).await {
        Ok(held) => Json(
            held.into_iter()
                .map(RetrieverView::from)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(error) => store_refusal(error),
    }
}

/// One retriever, or a report that this owner has none under the name.
async fn read(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
    Query(owned): Query<Owned>,
) -> Response<Body> {
    let id = match RetrieverId::new(&id) {
        Ok(id) => id,
        Err(error) => return *unprocessable(error),
    };
    let owner = match owner_of(&state, &manager, owned).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    match state.store().retriever(&owner, &id).await {
        Ok(Some(held)) => Json(RetrieverView::from(held)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Stores a retriever under the name the path gives it, replacing whatever was there.
///
/// Nothing is told to reload, as with an MCP server: a retriever is read when a run resolves the
/// tools a node was given, so the next run sees this one and no node holds a stale copy.
async fn put(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
    Query(owned): Query<Owned>,
    body: Bytes,
) -> Response<Body> {
    let id = match RetrieverId::new(&id) {
        Ok(id) => id,
        Err(error) => return *unprocessable(error),
    };
    let owner = match owner_of(&state, &manager, owned).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    let Json(written) = match Json::<Written>::from_bytes(&body) {
        Ok(written) => written,
        Err(rejection) => return rejection.into_response(),
    };
    // The one refusal a body cannot carry on its own: a collection has no edges, so whether the
    // tools fit is a question about the source beside them.
    let retriever = match Retriever::new(RetrieverParts {
        id,
        name: written.name,
        endpoint: written.endpoint,
        auth: written.auth,
        source: written.source,
        tools: written.tools,
        meta: written.meta,
        max_hits: written.max_hits,
    }) {
        Ok(retriever) => retriever,
        Err(error) => return *unprocessable(error),
    };
    let measured = match measure(&state, &retriever).await {
        Ok(measured) => measured,
        Err(refusal) => return *refusal,
    };
    let by = By::new(manager.user().clone(), ENDPOINT);
    match state
        .store()
        .put_retriever(NewRetriever { owner, retriever }, &by)
        .await
    {
        // The retriever as stored, so the caller learns when it was written without asking again.
        Ok(stored) => {
            let mut view = RetrieverView::from(stored);
            view.measured = measured;
            (StatusCode::CREATED, Json(view)).into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// What this deployment can say about the vectors a retriever's backend holds, before it is
/// stored, or what the caller is told instead.
///
/// **Asked here rather than at search time**, which is the whole of this: a query is embedded by
/// the one artefact this process holds, so a backend whose vectors are another width answers a
/// search with a bad answer rather than an error — nothing in a vector comparison says the two
/// came from different models. An operator learns it while writing the row.
///
/// Three things are refused, and each is the operator's to fix: a `search` in a deployment that
/// embeds nothing, a backend that answered about something other than a width, and a width that
/// disagrees with ours. One is not: a backend that cannot be reached at all is nobody's fault and
/// worth trying again, which is the 503 a plane gets everywhere else in this api.
///
/// Nothing is asked where no `search` is offered. The four graph tools walk edges and compare
/// nothing, so a plane offering only those is searched with no vector of ours and has no width to
/// agree with — which is also what lets a deployment with no artefact hold one.
async fn measure(state: &AppState, retriever: &Retriever) -> Result<Option<Measured>, Refusal> {
    if !retriever.tools().offers(RetrieverTool::Search) {
        return Ok(None);
    }
    let Some(ours) = state.embedder_width() else {
        return Err(unprocessable(
            "this deployment embeds nothing, so a retriever offering `search` could never be              asked: configure a `[semantic]` artefact, or offer only the tools that walk edges",
        ));
    };
    let backend = Backend::open(retriever).map_err(unanswered)?;
    match backend.width().await.map_err(unanswered)? {
        Some(width) if width != ours => Err(unprocessable(format!(
            "its vectors are {width} numbers wide and this deployment embeds a query into {ours},              so a search of it would compare two models' vectors"
        ))),
        Some(width) => Ok(Some(Measured {
            width: Some(width),
            note: "its vectors are the width this deployment embeds a query into",
        })),
        None => Ok(Some(Measured {
            width: None,
            note: "it holds no vector under that property yet, so nothing could disagree: a                    search answers nothing until it does, and nothing rather than an error if what                    it is embedded with turns out to be another model",
        })),
    }
}

/// What a caller is told when a retriever's own backend would not answer.
fn unanswered(error: Unanswered) -> Refusal {
    match error {
        Unanswered::Unreachable(said) => Box::new(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("its backend could not be reached: {said}"),
            )
                .into_response(),
        ),
        Unanswered::Refused(said) => unprocessable(said),
    }
}

/// Removes a retriever, or reports that this owner has none under the name.
async fn remove(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
    Query(owned): Query<Owned>,
) -> Response<Body> {
    let id = match RetrieverId::new(&id) {
        Ok(id) => id,
        Err(error) => return *unprocessable(error),
    };
    let owner = match owner_of(&state, &manager, owned).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    let by = By::new(manager.user().clone(), ENDPOINT);
    match state.store().remove_retriever(&owner, &id, &by).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Whose retrievers this caller may manage, or what it is told when it may manage none.
///
/// The same rule as the agent templates' and the graphs': a sysadmin manages anyone's and the
/// deployment's own, and anybody else manages one named tenant's and only with
/// [`Capability::SysAgent`] over it. The capability is deliberately not a new one, for the reason
/// a graph's is not — a retriever is what a template looks something up in, so whoever may write
/// the templates may describe what they read.
///
/// It governs reading as well as writing: an endpoint and a payload schema say where a tenant
/// keeps its documents, which is not something every member of it needs.
async fn owner_of(
    state: &AppState,
    manager: &Manager,
    owned: Owned,
) -> Result<RetrieverOwner, Refusal> {
    let named = owned
        .tenant
        .as_deref()
        .map(TenantId::new)
        .transpose()
        .map_err(unprocessable)?;

    let Some(named) = named else {
        return if manager.is_system_administrator() {
            Ok(RetrieverOwner::Global)
        } else {
            Err(Box::new(
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "name the tenant whose retrievers you manage",
                )
                    .into_response(),
            ))
        };
    };
    if manager.is_system_administrator() {
        return Ok(RetrieverOwner::Tenant(named));
    }
    let scope = CapabilityScope::Tenant {
        user: manager.user().clone(),
        tenant: named.clone(),
    };
    match manager
        .allows(state.store().as_ref(), Capability::SysAgent, &scope)
        .await
    {
        Ok(true) => Ok(RetrieverOwner::Tenant(named)),
        Ok(false) => Err(Box::new(StatusCode::FORBIDDEN.into_response())),
        Err(error) => Err(Box::new(store_refusal(error))),
    }
}

/// A value the caller spelled wrongly, reported as the caller's mistake rather than ours.
fn unprocessable<E: std::fmt::Display>(error: E) -> Refusal {
    Box::new((StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ip_core::{Grant, TnKey, UserId};
    use ip_drsg::{Replies, StubPlane};
    use ip_qdrant::{Answers, StubQdrant};
    use ip_retrieve::CollectionName;
    use ip_storage::{
        AccountKind, Membership, MembershipStore, NewTenant, NewUser, RetrieverStore, SqliteStore,
        Standing, TenantStore, UserStore,
    };
    use ip_storage::{AuditFilter, Entity, GrantStore, Page};
    use tower::ServiceExt;

    use super::super::harness::{body_of, cookie_of, hashed, request, state_over_shared};
    use super::*;

    /// A key no answer of ours may ever carry.
    const KEY: &str = "qd-live-9f2c4a7b1e";

    fn user(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    fn tenant(raw: &str) -> TenantId {
        TenantId::new(raw).unwrap()
    }

    fn by(actor: &str, through: &'static str) -> By {
        By::new(user(actor), through)
    }

    /// A retriever over a collection, requiring nothing.
    fn retriever(id: &str, name: &str) -> Retriever {
        Retriever::new(RetrieverParts {
            id: RetrieverId::new(id).unwrap(),
            name: RetrieverName::new(name).unwrap(),
            endpoint: RetrieverEndpoint::new("https://qdrant.acme.test:6333").unwrap(),
            auth: RetrieverAuth::None,
            source: Source::Qdrant {
                collection: CollectionName::new("handbook").unwrap(),
                vector: None,
            },
            tools: RetrieverTools::new(vec![RetrieverTool::Search]).unwrap(),
            meta: MetaSchema::default(),
            max_hits: MaxHits::default(),
        })
        .unwrap()
    }

    /// Two tenants, four accounts, and one retriever under each of the three owners.
    ///
    /// root administers, alice owns acme, bob is a member of it holding nothing, and carol owns
    /// other — so every refusal below has somebody it really belongs to.
    async fn store() -> Arc<SqliteStore> {
        let store = SqliteStore::in_memory().await.unwrap();
        for id in ["acme", "other"] {
            store
                .create_tenant(
                    NewTenant {
                        id: tenant(id),
                        key: TnKey::generate().unwrap(),
                    },
                    &by("root", "/_ip/tenants"),
                )
                .await
                .unwrap();
        }
        for (id, kind) in [
            ("root", AccountKind::SystemAdministrator),
            ("alice", AccountKind::Regular),
            ("bob", AccountKind::Regular),
            ("carol", AccountKind::Regular),
        ] {
            store
                .create_user(
                    NewUser {
                        id: user(id),
                        passphrase: hashed(),
                        kind,
                    },
                    &by("root", "/_ip/users"),
                )
                .await
                .unwrap();
        }
        for (id, tenant_id, standing) in [
            ("alice", "acme", Standing::Owner),
            ("bob", "acme", Standing::Member),
            ("carol", "other", Standing::Owner),
        ] {
            store
                .attach(
                    Membership {
                        user: user(id),
                        tenant: tenant(tenant_id),
                        standing,
                    },
                    &by("root", "/_ip/tenants/{tenant}/members"),
                )
                .await
                .unwrap();
        }
        for (owner, id, name) in [
            (RetrieverOwner::Global, "deployment", "The deployment's"),
            (
                RetrieverOwner::Tenant(tenant("acme")),
                "acme-handbook",
                "Acme's",
            ),
            (
                RetrieverOwner::Tenant(tenant("other")),
                "other-handbook",
                "Other's",
            ),
        ] {
            store
                .put_retriever(
                    NewRetriever {
                        owner,
                        retriever: retriever(id, name),
                    },
                    &by("root", ENDPOINT),
                )
                .await
                .unwrap();
        }
        Arc::new(store)
    }

    async fn fixture() -> (Router, AppState) {
        let state = state_over_shared(store().await);
        (super::super::router(state.clone()), state)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "a test fixture: one request, whose six parts a composite would only move"
    )]
    async fn send(
        router: &Router,
        state: &AppState,
        method: &str,
        caller: &str,
        uri: &str,
        body: Option<&str>,
    ) -> Response<Body> {
        router
            .clone()
            .oneshot(request(
                method,
                uri,
                &cookie_of(state, &user(caller)).await,
                body,
            ))
            .await
            .unwrap()
    }

    /// The ids a listing answered with, in the order it answered.
    async fn named(response: Response<Body>) -> Vec<String> {
        let body = body_of(response).await;
        serde_json::from_str::<Vec<serde_json::Value>>(&body)
            .unwrap_or_else(|error| panic!("{error}: {body}"))
            .into_iter()
            .map(|held| held["id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// A retriever as a caller writes one, without the id the path carries.
    ///
    /// It offers the tools that walk edges and no `search`, which is what keeps every test of who
    /// may manage what from needing a backend to answer: a search is the one tool whose width this
    /// deployment compares against its own, and the tests of that comparison are below, each with
    /// a stub of its own.
    fn written(name: &str) -> String {
        format!(
            r#"{{"name":"{name}","endpoint":"https://qdrant.acme.test:6333",
               "source":{{"kind":"drsg","plane":"handbook","label":"Document",
                          "property":"embedding"}},
               "tools":["neighbors","rank"],
               "meta":{{"team":"Which team the page belongs to"}},"max_hits":8}}"#
        )
    }

    #[tokio::test]
    async fn the_administrator_reads_what_the_deployment_holds() {
        let (router, state) = fixture().await;

        let response = send(&router, &state, "GET", "root", "/_ip/retrievers", None).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(named(response).await, ["deployment"]);
    }

    /// The rows, not the status code: a caller admitted and then handed too much is what the
    /// isolation cases cannot see.
    #[tokio::test]
    async fn an_owner_reads_only_the_retrievers_of_its_own_tenant() {
        let (router, state) = fixture().await;

        let response = send(
            &router,
            &state,
            "GET",
            "alice",
            "/_ip/retrievers?tenant=acme",
            None,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(named(response).await, ["acme-handbook"]);
    }

    #[tokio::test]
    async fn the_administrator_may_name_a_tenant_and_read_its_retrievers() {
        let (router, state) = fixture().await;

        let response = send(
            &router,
            &state,
            "GET",
            "root",
            "/_ip/retrievers?tenant=other",
            None,
        )
        .await;

        assert_eq!(named(response).await, ["other-handbook"]);
    }

    #[tokio::test]
    async fn a_member_holding_nothing_is_refused() {
        let (router, state) = fixture().await;

        let response = send(
            &router,
            &state,
            "GET",
            "bob",
            "/_ip/retrievers?tenant=acme",
            None,
        )
        .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_member_granted_the_capability_reads_them() {
        let held = store().await;
        held.grant(
            &Grant::new(
                Capability::SysAgent,
                CapabilityScope::Tenant {
                    user: user("bob"),
                    tenant: tenant("acme"),
                },
            )
            .unwrap(),
            &by("alice", "/_ip/tenants/{tenant}/members/{user}/grants"),
        )
        .await
        .unwrap();
        let state = state_over_shared(held);
        let router = super::super::router(state.clone());

        let response = send(
            &router,
            &state,
            "GET",
            "bob",
            "/_ip/retrievers?tenant=acme",
            None,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(named(response).await, ["acme-handbook"]);
    }

    #[tokio::test]
    async fn an_owner_of_one_tenant_is_refused_another() {
        let (router, state) = fixture().await;

        let response = send(
            &router,
            &state,
            "GET",
            "carol",
            "/_ip/retrievers?tenant=acme",
            None,
        )
        .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// Nobody but the administrator manages what the whole deployment holds.
    #[tokio::test]
    async fn a_member_naming_no_tenant_is_told_to_name_one() {
        let (router, state) = fixture().await;

        let response = send(&router, &state, "GET", "alice", "/_ip/retrievers", None).await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_retriever_is_read_back_with_its_meta_schema_intact() {
        let (router, state) = fixture().await;

        let written = send(
            &router,
            &state,
            "PUT",
            "alice",
            "/_ip/retrievers/handbook?tenant=acme",
            Some(&written("The handbook")),
        )
        .await;
        assert_eq!(written.status(), StatusCode::CREATED);

        let read = send(
            &router,
            &state,
            "GET",
            "alice",
            "/_ip/retrievers/handbook?tenant=acme",
            None,
        )
        .await;
        assert_eq!(read.status(), StatusCode::OK);
        let held: serde_json::Value = serde_json::from_str(&body_of(read).await).unwrap();
        assert_eq!(held["id"], "handbook");
        assert_eq!(held["tenant"], "acme");
        assert_eq!(held["source"]["kind"], "drsg");
        assert_eq!(held["source"]["plane"], "handbook");
        assert_eq!(held["tools"], serde_json::json!(["neighbors", "rank"]));
        assert_eq!(held["meta"]["team"], "Which team the page belongs to");
        assert_eq!(held["max_hits"], 8);
        assert_eq!(held["auth"], "none");
        assert!(held["created_at"].is_i64());
    }

    /// The same fixture over a deployment that embeds a query into `width` numbers.
    async fn embedding(width: u32) -> (Router, AppState) {
        let state = state_over_shared(store().await).with_embedder_width(width);
        (super::super::router(state.clone()), state)
    }

    /// A retriever that offers a search, which is the one tool a width is compared for.
    fn seeking(endpoint: &str, source: serde_json::Value) -> String {
        serde_json::json!({
            "name": "The handbook",
            "endpoint": endpoint,
            "source": source,
            "tools": ["search"],
        })
        .to_string()
    }

    fn collection() -> serde_json::Value {
        serde_json::json!({"kind": "qdrant", "collection": "handbook"})
    }

    /// Writes one, and answers what the api said about it.
    async fn register(router: &Router, state: &AppState, body: &str) -> (StatusCode, String) {
        let answered = send(
            router,
            state,
            "PUT",
            "alice",
            "/_ip/retrievers/handbook?tenant=acme",
            Some(body),
        )
        .await;
        let status = answered.status();
        (status, body_of(answered).await)
    }

    /// Nothing could ever ask it, so it is refused while the operator is here to read why.
    #[tokio::test]
    async fn a_search_in_a_deployment_that_embeds_nothing_is_refused() {
        let (router, state) = fixture().await;
        let (status, said) = register(
            &router,
            &state,
            &seeking("https://qdrant.acme.test:6333", collection()),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(said.contains("embeds nothing"), "{said}");
        assert!(said.contains("semantic"), "{said}");
    }

    /// Ruling 2, and the reason it is a registration and not a search: two widths are two models,
    /// and comparing their vectors answers a wrong answer rather than an error.
    #[tokio::test]
    async fn a_collection_whose_vectors_are_another_models_is_refused() {
        let stub = StubQdrant::start(Answers::Collection {
            vectors: serde_json::json!({"size": 768, "distance": "Cosine"}),
        })
        .await;
        let (router, state) = embedding(384).await;

        let (status, said) = register(
            &router,
            &state,
            &seeking(&format!("http://{}", stub.address()), collection()),
        )
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(said.contains("768") && said.contains("384"), "{said}");
        assert!(
            state
                .store()
                .retriever(
                    &RetrieverOwner::Tenant(tenant("acme")),
                    &RetrieverId::new("handbook").unwrap()
                )
                .await
                .unwrap()
                .is_none(),
            "a refused retriever was stored anyway"
        );
    }

    /// What the api says it measured, and that it says it on the write and on nothing after.
    #[tokio::test]
    async fn a_collection_of_the_right_width_is_registered_and_says_what_was_measured() {
        let stub = StubQdrant::start(Answers::Collection {
            vectors: serde_json::json!({"size": 384, "distance": "Cosine"}),
        })
        .await;
        let (router, state) = embedding(384).await;

        let (status, said) = register(
            &router,
            &state,
            &seeking(&format!("http://{}", stub.address()), collection()),
        )
        .await;

        assert_eq!(status, StatusCode::CREATED, "{said}");
        let answered: serde_json::Value = serde_json::from_str(&said).unwrap();
        assert_eq!(answered["measured"]["width"], 384);
        assert!(
            answered["measured"]["note"]
                .as_str()
                .unwrap()
                .contains("width this deployment"),
            "{said}"
        );
        assert_eq!(stub.only().path, "/collections/handbook");

        let read = send(
            &router,
            &state,
            "GET",
            "alice",
            "/_ip/retrievers/handbook?tenant=acme",
            None,
        )
        .await;
        let held: serde_json::Value = serde_json::from_str(&body_of(read).await).unwrap();
        assert!(
            held.get("measured").is_none(),
            "a read reported a measurement nothing took: {held}"
        );
    }

    /// The case the plan calls out: a plane holds no width to report, and one holding no vector
    /// yet has nothing to disagree with — so it is registered, and the api says as much rather
    /// than leaving an operator to find out mid-run.
    #[tokio::test]
    async fn a_plane_with_nothing_to_measure_is_registered_and_says_so() {
        let stub =
            StubPlane::start(Replies::scripted([("plane.query", serde_json::json!([]))])).await;
        let (router, state) = embedding(384).await;

        let (status, said) = register(
            &router,
            &state,
            &seeking(
                &format!("http://{}/rpc", stub.address()),
                serde_json::json!({
                    "kind": "drsg", "plane": "handbook", "property": "embedding",
                }),
            ),
        )
        .await;

        assert_eq!(status, StatusCode::CREATED, "{said}");
        let answered: serde_json::Value = serde_json::from_str(&said).unwrap();
        assert!(
            answered["measured"].get("width").is_none(),
            "a plane holding nothing reported a width: {said}"
        );
        assert!(
            answered["measured"]["note"]
                .as_str()
                .unwrap()
                .contains("no vector"),
            "{said}"
        );
        assert_eq!(stub.only().method, "plane.query");
    }

    /// A backend nothing answers on is nobody's fault and worth trying again, which is the one
    /// refusal here that is not the operator's body to fix.
    #[tokio::test]
    async fn a_backend_that_cannot_be_reached_is_unavailable() {
        let (router, state) = embedding(384).await;
        let (status, said) = register(
            &router,
            &state,
            // A port nothing listens on, which is what a typo in an endpoint looks like.
            &seeking("http://127.0.0.1:1", collection()),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(said.contains("could not be reached"), "{said}");
    }

    /// The four graph tools compare nothing of ours, so a plane offering only those is registered
    /// without being asked anything — which is also what lets a deployment with no artefact hold
    /// one at all.
    #[tokio::test]
    async fn a_retriever_offering_no_search_is_registered_without_its_backend_being_asked() {
        let stub =
            StubPlane::start(Replies::scripted([("plane.query", serde_json::json!([]))])).await;
        let (router, state) = fixture().await;

        let (status, said) = register(
            &router,
            &state,
            &serde_json::json!({
                "name": "The handbook",
                "endpoint": format!("http://{}/rpc", stub.address()),
                "source": {"kind": "drsg", "plane": "handbook", "property": "embedding"},
                "tools": ["neighbors", "rank", "relate", "cluster"],
            })
            .to_string(),
        )
        .await;

        assert_eq!(status, StatusCode::CREATED, "{said}");
        let answered: serde_json::Value = serde_json::from_str(&said).unwrap();
        assert!(
            answered.get("measured").is_none(),
            "nothing was measured, so nothing should be reported: {said}"
        );
        assert!(
            stub.seen().is_empty(),
            "a plane was asked about a retriever that compares nothing: {:?}",
            stub.seen()
        );
    }

    /// The whole point of the view's shape: a key goes in and nothing ever answers with it.
    #[tokio::test]
    async fn a_key_is_never_answered_back() {
        let (router, state) = fixture().await;
        let body = format!(
            r#"{{"name":"The handbook","endpoint":"https://qdrant.acme.test:6333",
               "auth":{{"kind":"key","key":"{KEY}"}},
               "source":{{"kind":"drsg","plane":"handbook","property":"embedding"}},
               "tools":["rank"]}}"#
        );

        let created = send(
            &router,
            &state,
            "PUT",
            "alice",
            "/_ip/retrievers/handbook?tenant=acme",
            Some(&body),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        let answered = body_of(created).await;
        assert!(!answered.contains(KEY), "the answer carries it: {answered}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&answered).unwrap()["auth"] == "key",
            "{answered}"
        );

        for uri in [
            "/_ip/retrievers/handbook?tenant=acme",
            "/_ip/retrievers?tenant=acme",
        ] {
            let read = send(&router, &state, "GET", "alice", uri, None).await;
            let said = body_of(read).await;
            assert!(!said.contains(KEY), "{uri} carries it: {said}");
        }

        // Nor does the trail, which is the one other place a credential could settle.
        let trail = state
            .store()
            .audits(&AuditFilter::default(), Page::new(50, 0, None))
            .await
            .unwrap();
        let recorded = serde_json::to_string(&trail.len()).unwrap() + &format!("{trail:?}");
        assert!(!recorded.contains(KEY), "the trail carries it");
        assert!(
            trail.iter().any(|one| one.entity == Entity::Retriever),
            "the write was recorded at all"
        );
    }

    /// A collection has no edges, so a graph tool on one is the caller's mistake and not a call
    /// that fails later, in the middle of a run somebody is paying for.
    #[tokio::test]
    async fn a_collection_offering_a_graph_tool_is_the_callers_mistake() {
        let (router, state) = fixture().await;
        let body = r#"{"name":"The handbook","endpoint":"https://qdrant.acme.test:6333",
               "source":{"kind":"qdrant","collection":"handbook"},
               "tools":["search","cluster"]}"#;

        let response = send(
            &router,
            &state,
            "PUT",
            "alice",
            "/_ip/retrievers/handbook?tenant=acme",
            Some(body),
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let said = body_of(response).await;
        assert!(said.contains("cluster"), "{said} must name what it refused");
    }

    #[tokio::test]
    async fn a_body_this_build_does_not_carry_is_refused() {
        let (router, state) = fixture().await;
        for body in [
            // A field no retriever has.
            r#"{"name":"X","endpoint":"https://q.test","source":{"kind":"qdrant","collection":"c"},"tools":["search"],"nearness":0.9}"#,
            // A backend this deployment does not speak.
            r#"{"name":"X","endpoint":"https://q.test","source":{"kind":"milvus","collection":"c"},"tools":["search"]}"#,
            // Offering nothing at all.
            r#"{"name":"X","endpoint":"https://q.test","source":{"kind":"qdrant","collection":"c"},"tools":[]}"#,
            // A ceiling past what one look-up may answer with.
            r#"{"name":"X","endpoint":"https://q.test","source":{"kind":"qdrant","collection":"c"},"tools":["search"],"max_hits":900}"#,
            // An endpoint this gateway cannot call.
            r#"{"name":"X","endpoint":"ftp://q.test","source":{"kind":"qdrant","collection":"c"},"tools":["search"]}"#,
        ] {
            let response = send(
                &router,
                &state,
                "PUT",
                "alice",
                "/_ip/retrievers/handbook?tenant=acme",
                Some(body),
            )
            .await;

            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{body} was taken"
            );
        }
    }

    #[tokio::test]
    async fn an_id_that_is_not_one_is_the_callers_mistake() {
        let (router, state) = fixture().await;

        let response = send(
            &router,
            &state,
            "GET",
            "alice",
            "/_ip/retrievers/not%20an%20id?tenant=acme",
            None,
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn removing_one_leaves_the_others_and_removing_it_again_reports_it_missing() {
        let (router, state) = fixture().await;
        send(
            &router,
            &state,
            "PUT",
            "alice",
            "/_ip/retrievers/tickets?tenant=acme",
            Some(&written("The tickets")),
        )
        .await;

        let removed = send(
            &router,
            &state,
            "DELETE",
            "alice",
            "/_ip/retrievers/tickets?tenant=acme",
            None,
        )
        .await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);

        let read = send(
            &router,
            &state,
            "GET",
            "alice",
            "/_ip/retrievers/tickets?tenant=acme",
            None,
        )
        .await;
        assert_eq!(read.status(), StatusCode::NOT_FOUND);

        let again = send(
            &router,
            &state,
            "DELETE",
            "alice",
            "/_ip/retrievers/tickets?tenant=acme",
            None,
        )
        .await;
        assert_eq!(again.status(), StatusCode::NOT_FOUND);

        let left = send(
            &router,
            &state,
            "GET",
            "alice",
            "/_ip/retrievers?tenant=acme",
            None,
        )
        .await;
        assert_eq!(named(left).await, ["acme-handbook"]);
    }

    /// Every store call a request makes carries its own "the store could not answer" arm,
    /// and a database that works reaches none of them. Failing each in turn reaches them all.
    ///
    /// The sweep starts at the first call of the whole request — the extractor's own read of
    /// the caller's standing — and the count is asserted, so a handler that gains or loses a
    /// store call fails here rather than quietly leaving a new arm untested. Each refused
    /// request changes nothing, which is what lets one fixture serve a whole sweep.
    #[tokio::test]
    async fn a_store_that_cannot_answer_is_the_server_s_own_failure() {
        let (router, state) = fixture().await;
        let cookie = cookie_of(&state, &user("alice")).await;
        for (method, uri, body, calls) in [
            ("GET", "/_ip/retrievers?tenant=acme", None, 4),
            ("GET", "/_ip/retrievers/acme-handbook?tenant=acme", None, 4),
            (
                "PUT",
                "/_ip/retrievers/acme-handbook?tenant=acme",
                Some(written("Acme's").as_str().to_owned()),
                4,
            ),
            (
                "DELETE",
                "/_ip/retrievers/acme-handbook?tenant=acme",
                None,
                4,
            ),
        ] {
            let mut refused = 0;

            loop {
                state.store_failure().answer_only(refused);
                let response = router
                    .clone()
                    .oneshot(request(method, uri, &cookie, body.as_deref()))
                    .await
                    .unwrap();
                state.store_failure().answer_again();
                if response.status() != StatusCode::INTERNAL_SERVER_ERROR {
                    break;
                }
                refused += 1;
                assert!(refused <= 9, "{method} {uri} refuses whatever it is given");
            }

            assert_eq!(refused, calls, "{method} {uri}");
        }
    }
}
