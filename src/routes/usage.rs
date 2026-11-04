use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{FromRequestParts, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ip_core::{
    ApiId, Capability, CapabilityScope, Decider, Difficulty, Hedge, Latency, Money, Served,
    TenantId, Timestamp, UserId,
};
use ip_storage::{Decided, SpentArms, Usage, UsageFilter};

use super::{Paged, store_refusal};
use crate::manage::Manager;
use crate::state::AppState;

/// A refusal, boxed so the ordinary answer is not paid for by every caller.
type Refusal = Box<Response<Body>>;

/// What every request cost, under the prefix the router nests it at.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/summary", get(summary))
}

/// One recorded request, as the api reports it.
#[derive(Debug, serde::Serialize)]
pub struct UsageView {
    /// The trace the request was followed under, in the hex the answer carried.
    pub trace: String,
    /// The chat turn it belonged to, when the caller marked one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    /// Who it was spent for.
    pub tenant: String,
    /// Which account made it.
    pub user: String,
    /// Which api served it.
    pub api: String,
    /// The model that answered, when the answer named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Tokens the request carried to the model.
    pub input_tokens: u32,
    /// Tokens the model answered with.
    pub output_tokens: u32,
    /// `upstream` when the model answered, `cache` when nothing was spent.
    pub served: Served,
    /// Which endpoint behind the rule answered, as `host:port`, absent for an answer the
    /// cache gave, which reached no endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub served_by: Option<String>,
    /// How many endpoints behind the rule it was raced across; one is what it was before
    /// anything raced.
    pub raced_across: Hedge,
    /// The difficulty class the request was dispatched at, absent when nothing named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatched_class: Option<Difficulty>,
    /// Which level of the margin named that class — the request, its turn's memory, the
    /// local classifier or the arbiter — absent where there is no class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<Decider>,
    /// How many secrets the filter took out of the request and its answer, absent
    /// where no filter looked at them. Nought means it looked and found none, which
    /// is a different fact from not having looked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted: Option<u32>,
    /// What those were, as the filter named them, and never what they said.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted_kinds: Option<String>,
    /// How long the caller waited, in milliseconds.
    pub latency_ms: u32,
    /// What it cost, in micros of the deployment's currency, absent where nothing priced
    /// the model. Absent is not nought: a model priced at nothing reports nought.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
    /// When it was recorded, in seconds since the unix epoch.
    pub created_at: Timestamp,
}

impl From<Usage> for UsageView {
    fn from(usage: Usage) -> Self {
        Self {
            trace: usage.trace.to_hex(),
            turn: usage.turn.map(|turn| turn.to_string()),
            tenant: usage.tenant.to_string(),
            user: usage.user.to_string(),
            api: usage.api.to_string(),
            model: usage.model.map(|model| model.to_string()),
            input_tokens: usage.tokens.input.get(),
            output_tokens: usage.tokens.output.get(),
            served: usage.served,
            served_by: usage.served_by.map(|replica| replica.to_string()),
            raced_across: usage.raced_across,
            dispatched_class: usage.dispatched_class,
            decided_by: usage.decided_by,
            redacted: usage.redacted.count(),
            redacted_kinds: usage.redacted.kinds().map(str::to_owned),
            latency_ms: Latency::millis(usage.latency),
            cost_micros: usage.cost.map(Money::micros),
            created_at: usage.created_at,
        }
    }
}

/// What a window of requests answered and cost, as the api reports it. Both numbers raw:
/// a ratio, a percentage or a "saving" is one subtraction the caller makes in whichever
/// frame fits the question, never one this api invents.
#[derive(Debug, serde::Serialize)]
pub struct SpendView {
    /// Requests answered — every row in the window, an answer out of the cache included.
    pub answered: u64,
    /// Requests that went upstream: the arms the rows the upstream answered carried.
    /// Past `answered` by exactly what hedging spent, and no cache hit inflates it.
    pub sent: u64,
    /// How many of the window's requests each level of the margin decided, and how many
    /// nothing could. The five add up to `answered`.
    pub decided: DecidedView,
    /// What the window cost, in micros of the deployment's currency, over the rows that
    /// carry a cost.
    pub cost_micros: u64,
    /// How many of the window's requests nothing priced, and which `cost_micros` is
    /// therefore missing. Nought means the total is of everything answered.
    pub unpriced: u64,
    /// What the window was asked open from, in seconds since the unix epoch; null when
    /// it opened with everything.
    pub since: Option<i64>,
}

/// How many requests of a window each level of the margin decided, as the api reports
/// it. Counts and nothing else: what the local model carried and what the arbiter cost
/// are the caller's subtractions, in whichever frame fits the question.
#[derive(Debug, serde::Serialize)]
pub struct DecidedView {
    /// Requests that named their own class.
    pub request: u64,
    /// Requests dispatched at the class their turn remembered.
    pub turn: u64,
    /// Openings the local classifier judged — its real call rate, with no remembered
    /// verdict inflating it.
    pub classifier: u64,
    /// Openings the arbiter was paid to rule on.
    pub arbiter: u64,
    /// Requests nothing could decide, dispatched unclassified.
    pub undecided: u64,
}

impl From<Decided> for DecidedView {
    fn from(decided: Decided) -> Self {
        Self {
            request: decided.request,
            turn: decided.turn,
            classifier: decided.classifier,
            arbiter: decided.arbiter,
            undecided: decided.undecided,
        }
    }
}

impl SpendView {
    /// The two sums over an open window.
    fn everything(sums: SpentArms) -> Self {
        Self {
            answered: sums.answered,
            sent: sums.sent,
            decided: sums.decided.into(),
            cost_micros: sums.cost.micros(),
            unpriced: sums.unpriced,
            since: None,
        }
    }

    /// The two sums over a window that opens at `since`.
    fn since(sums: SpentArms, since: Timestamp) -> Self {
        Self {
            answered: sums.answered,
            sent: sums.sent,
            decided: sums.decided.into(),
            cost_micros: sums.cost.micros(),
            unpriced: sums.unpriced,
            since: Some(since.unix_seconds()),
        }
    }
}

/// What was answered and what it cost upstream, for one narrowing of one window — the
/// summary of what the list spells out.
///
/// The same authorization as the list, the same rules for naming a tenant, the same meaning
/// for an absent `after`, and no paging: a sum has no page, so of the list's query only
/// `after` is read — and the whole `Paged` is taken anyway, so the window cannot drift.
async fn summary(
    State(state): State<AppState>,
    manager: Manager,
    Narrowed(narrowing): Narrowed,
    Paged(page): Paged,
) -> Response<Body> {
    let filter = match filter_for(&state, &manager, narrowing).await {
        Ok(filter) => filter,
        Err(refusal) => return *refusal,
    };
    // The window opens where the list's page opens: no moment named, no floor kept.
    match state.store().sent_upstream(&filter, page.after()).await {
        Ok(sums) => match page.after() {
            Some(after) => Json(SpendView::since(sums, after)).into_response(),
            None => Json(SpendView::everything(sums)).into_response(),
        },
        Err(error) => store_refusal(error),
    }
}

/// What a caller narrows the list to, on top of what it is allowed to see at all.
#[derive(Debug, Default, serde::Deserialize)]
struct Narrowing {
    tenant: Option<String>,
    user: Option<String>,
    api: Option<String>,
}

/// What was spent, newest first.
///
/// The system administrator sees every tenant, and may name one. Anyone else names the tenant
/// they are asking about and must hold `Observer` inside it, which a tenant owner does by
/// standing: a caller only ever reads the tenant whose name it asked for.
async fn list(
    State(state): State<AppState>,
    manager: Manager,
    Narrowed(narrowing): Narrowed,
    Paged(page): Paged,
) -> Response<Body> {
    let filter = match filter_for(&state, &manager, narrowing).await {
        Ok(filter) => filter,
        Err(refusal) => return *refusal,
    };
    match state.store().list_usage(&filter, page).await {
        Ok(listed) => {
            Json(listed.into_iter().map(UsageView::from).collect::<Vec<_>>()).into_response()
        }
        Err(error) => store_refusal(error),
    }
}

/// What this caller may read, narrowed by what it asked for.
async fn filter_for(
    state: &AppState,
    manager: &Manager,
    narrowing: Narrowing,
) -> Result<UsageFilter, Refusal> {
    let tenant = narrowing
        .tenant
        .as_deref()
        .map(TenantId::new)
        .transpose()
        .map_err(|error| {
            Box::new((StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response())
        })?;
    let user = narrowing
        .user
        .as_deref()
        .map(UserId::new)
        .transpose()
        .map_err(|error| {
            Box::new((StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response())
        })?;
    let api = narrowing
        .api
        .as_deref()
        .map(ApiId::new)
        .transpose()
        .map_err(|error| {
            Box::new((StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response())
        })?;

    if !manager.is_system_administrator() {
        let Some(named) = tenant.clone() else {
            return Err(Box::new(
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "name the tenant to read the usage of",
                )
                    .into_response(),
            ));
        };
        let scope = CapabilityScope::Tenant {
            user: manager.user().clone(),
            tenant: named,
        };
        match manager
            .allows(state.store().as_ref(), Capability::Observer, &scope)
            .await
        {
            Ok(true) => {}
            Ok(false) => return Err(Box::new(StatusCode::FORBIDDEN.into_response())),
            Err(error) => return Err(Box::new(store_refusal(error))),
        }
    }
    Ok(UsageFilter { tenant, user, api })
}

/// The `tenant`, `user` and `api` a caller narrows the list by.
struct Narrowed(Narrowing);

impl<S: Send + Sync> FromRequestParts<S> for Narrowed {
    type Rejection = Response<Body>;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(narrowing) = Query::<Narrowing>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        Ok(Self(narrowing))
    }
}

#[cfg(test)]
mod tests {
    use ip_core::{Grant, Hedge, ModelName, TnKey, TokenCount, Tokens, TraceId, TurnId};
    use ip_storage::{
        AccountKind, GrantStore, Membership, MembershipStore, NewTenant, NewUsage, NewUser,
        SqliteStore, Standing, TenantStore, UsageStore, UserStore,
    };
    use tower::ServiceExt;

    use super::super::harness::{body_of, cookie_of, hashed, request, state_over_shared};
    use super::*;
    use axum::Router;
    use std::sync::Arc;

    fn user(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    fn tenant(raw: &str) -> TenantId {
        TenantId::new(raw).unwrap()
    }

    /// One request, spent by whoever is named.
    fn spent(tenant_id: &str, user_id: &str, api: &str) -> NewUsage {
        NewUsage {
            trace: TraceId::generate().unwrap(),
            turn: Some(TurnId::new("turn-1").unwrap()),
            tenant: tenant(tenant_id),
            user: user(user_id),
            api: ApiId::new(api).unwrap(),
            model: Some(ModelName::new("claude-opus-5").unwrap()),
            tokens: Tokens::new(TokenCount::new(120), TokenCount::new(30)),
            served: Served::Upstream,
            served_by: Some(ip_core::Replica {
                host: ip_core::Host::new("one.example.com").unwrap(),
                port: ip_core::Port::new(443).unwrap(),
            }),
            raced_across: Hedge::default(),
            dispatched_class: None,
            decided_by: None,
            redacted: ip_core::Redacted::NONE,
            latency: Latency::from_millis(1_250),
            cost: None,
        }
    }

    /// Two tenants, each having spent something: acme, owned by alice with bob a member, and
    /// other, owned by carol. root is the system administrator.
    async fn store() -> Arc<SqliteStore> {
        let store = SqliteStore::in_memory().await.unwrap();
        for id in ["acme", "other"] {
            store
                .create_tenant(
                    NewTenant {
                        id: tenant(id),
                        key: TnKey::generate().unwrap(),
                    },
                    &ip_storage::By::new(ip_core::UserId::new("root").unwrap(), "/_ip/tenants"),
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
                    &ip_storage::By::new(ip_core::UserId::new("root").unwrap(), "/_ip/users"),
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
                    &ip_storage::By::new(
                        ip_core::UserId::new("root").unwrap(),
                        "/_ip/tenants/{tenant}/members",
                    ),
                )
                .await
                .unwrap();
        }
        store
            .record_usage(spent("acme", "alice", "anthropic"))
            .await
            .unwrap();
        store
            .record_usage(spent("acme", "bob", "openai"))
            .await
            .unwrap();
        store
            .record_usage(spent("other", "carol", "anthropic"))
            .await
            .unwrap();
        Arc::new(store)
    }

    async fn read(router: &Router, state: &AppState, caller: &str, query: &str) -> Response<Body> {
        read_at(router, state, caller, "/_ip/usage", query).await
    }

    async fn read_at(
        router: &Router,
        state: &AppState,
        caller: &str,
        path: &str,
        query: &str,
    ) -> Response<Body> {
        router
            .clone()
            .oneshot(request(
                "GET",
                &format!("{path}{query}"),
                &cookie_of(state, &user(caller)).await,
                None,
            ))
            .await
            .unwrap()
    }

    async fn listed(response: Response<Body>) -> serde_json::Value {
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_str(&body_of(response).await).unwrap()
    }

    #[tokio::test]
    async fn the_administrator_reads_what_every_tenant_spent() {
        let store = store().await;
        // Recorded now, so it is the newest row and comes back first from the listing.
        let count = NewUsage {
            tenant: tenant("other"),
            user: user("dave"),
            api: ApiId::new("anthropic").unwrap(),
            raced_across: Hedge::new(2).unwrap(),
            dispatched_class: Some(Difficulty::Hard),
            decided_by: Some(Decider::Classifier),
            ..spent("other", "dave", "anthropic")
        };
        store.record_usage(count).await.unwrap();
        let state = state_over_shared(store);
        let router = crate::routes::router(state.clone());
        let listed = listed(read(&router, &state, "root", "").await).await;
        assert_eq!(listed.as_array().unwrap().len(), 4);

        let row = &listed[0];
        assert_eq!(row["tenant"], "other");
        assert_eq!(row["user"], "dave");
        assert_eq!(row["api"], "anthropic");
        assert_eq!(row["model"], "claude-opus-5");
        assert_eq!(row["input_tokens"], 120);
        assert_eq!(row["output_tokens"], 30);
        assert_eq!(row["served"], "upstream");
        assert_eq!(row["served_by"], "one.example.com:443");
        assert_eq!(row["raced_across"], 2);
        assert_eq!(row["dispatched_class"], "hard");
        assert_eq!(row["decided_by"], "classifier");
        assert_eq!(row["latency_ms"], 1250);
        assert_eq!(row["turn"], "turn-1");
        assert!(row["created_at"].is_number(), "a time is not a number");
        assert_eq!(row["trace"].as_str().unwrap().len(), 32);

        // A default row still renders as one, that both a raced count and the ordinary one
        // reach the view, and one without a class renders without the key at all rather than
        // with a null in it.
        assert_eq!(listed.as_array().unwrap()[1]["raced_across"], 1);
        assert!(
            listed.as_array().unwrap()[1]
                .get("dispatched_class")
                .is_none()
        );
        assert!(listed.as_array().unwrap()[1].get("decided_by").is_none());
    }

    #[tokio::test]
    async fn the_administrator_may_name_one_tenant() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let listed = listed(read(&router, &state, "root", "?tenant=acme").await).await;
        assert_eq!(listed.as_array().unwrap().len(), 2);
        assert!(
            listed
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["tenant"] == "acme")
        );
    }

    #[tokio::test]
    async fn an_owner_reads_its_own_tenant() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let listed = listed(read(&router, &state, "alice", "?tenant=acme").await).await;
        assert_eq!(listed.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_owner_reads_no_other_tenant() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = read(&router, &state, "alice", "?tenant=other").await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_member_reads_nothing_until_it_is_granted_observer() {
        let store = store().await;
        let state = state_over_shared(Arc::clone(&store));
        let router = crate::routes::router(state.clone());
        let refused = read(&router, &state, "bob", "?tenant=acme").await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);

        store
            .grant(
                &Grant::new(
                    Capability::Observer,
                    CapabilityScope::Tenant {
                        user: user("bob"),
                        tenant: tenant("acme"),
                    },
                )
                .unwrap(),
                &ip_storage::By::new(
                    ip_core::UserId::new("root").unwrap(),
                    "/_ip/tenants/{tenant}/members/{user}/grants",
                ),
            )
            .await
            .unwrap();
        let listed = listed(read(&router, &state, "bob", "?tenant=acme").await).await;
        assert_eq!(listed.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn anyone_but_the_administrator_names_the_tenant_it_is_asking_about() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = read(&router, &state, "alice", "").await;
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_list_is_narrowed_by_user_and_by_api() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let by_user = listed(read(&router, &state, "alice", "?tenant=acme&user=bob").await).await;
        assert_eq!(by_user.as_array().unwrap().len(), 1);
        assert_eq!(by_user[0]["user"], "bob");

        let by_api =
            listed(read(&router, &state, "alice", "?tenant=acme&api=anthropic").await).await;
        assert_eq!(by_api.as_array().unwrap().len(), 1);
        assert_eq!(by_api[0]["user"], "alice");
    }

    #[tokio::test]
    async fn a_page_holds_what_it_was_asked_for() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let first = listed(read(&router, &state, "root", "?limit=2").await).await;
        assert_eq!(first.as_array().unwrap().len(), 2);
        let second = listed(read(&router, &state, "root", "?limit=2&offset=2").await).await;
        assert_eq!(second.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_name_that_is_not_one_is_refused_rather_than_ignored() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = read(&router, &state, "root", "?tenant=NOT%20A%20TENANT").await;
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// What one window answered and cost, for whoever the query names.
    async fn summary_of(
        router: &Router,
        state: &AppState,
        caller: &str,
        query: &str,
    ) -> serde_json::Value {
        let read = read_at(router, state, caller, "/_ip/usage/summary", query).await;
        let body = body_of(read).await;
        eprintln!("BODY {body}");
        serde_json::from_str(&body).unwrap()
    }

    #[tokio::test]
    async fn a_tenant_reads_its_own_spend() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let sums = summary_of(&router, &state, "alice", "?tenant=acme").await;
        assert_eq!(sums["answered"], 2);
        assert_eq!(sums["sent"], 2);
        assert_eq!(
            sums["decided"]["undecided"], 2,
            "the counts are narrowed to the tenant like everything else"
        );
        assert!(sums["since"].is_null(), "an open window grew a floor");
    }

    #[tokio::test]
    async fn the_administrator_reads_another_tenants_spend() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let sums = summary_of(&router, &state, "root", "?tenant=other").await;
        assert_eq!(
            (sums["answered"].as_u64(), sums["sent"].as_u64()),
            (Some(1), Some(1))
        );
        let all = summary_of(&router, &state, "root", "").await;
        assert_eq!(all["answered"], 3);
    }

    #[tokio::test]
    async fn a_tenant_may_not_read_another_tenants_spend() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = read(&router, &state, "alice", "?tenant=other").await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    }

    /// One request, priced at `micros`, under whoever is named.
    fn costing(tenant_id: &str, user_id: &str, api: &str, micros: u64) -> NewUsage {
        NewUsage {
            cost: Some(Money::from_micros(micros)),
            ..spent(tenant_id, user_id, api)
        }
    }

    #[tokio::test]
    async fn a_listed_row_says_what_it_cost_and_an_unpriced_one_says_nothing() {
        let store = store().await;
        store
            .record_usage(costing("acme", "alice", "gemini", 810))
            .await
            .unwrap();
        let state = state_over_shared(store);
        let router = crate::routes::router(state.clone());

        let priced = listed(read(&router, &state, "root", "?api=gemini").await).await;
        assert_eq!(priced[0]["cost_micros"], 810);

        // A row nothing priced carries no key at all, rather than a null or a nought.
        let unpriced = listed(read(&router, &state, "root", "?api=openai").await).await;
        assert!(unpriced[0].get("cost_micros").is_none());
    }

    #[tokio::test]
    async fn a_summary_totals_what_the_window_cost() {
        let store = store().await;
        for micros in [810, 1_530, 0] {
            store
                .record_usage(costing("acme", "alice", "gemini", micros))
                .await
                .unwrap();
        }
        let state = state_over_shared(store);
        let router = crate::routes::router(state.clone());

        let sums = summary_of(&router, &state, "root", "?api=gemini").await;
        assert_eq!(sums["answered"], 3);
        assert_eq!(sums["cost_micros"], 2_340);
        assert_eq!(sums["unpriced"], 0, "every row of the window was priced");
    }

    /// A total over rows of which some are unpriced says how many it could not price,
    /// rather than reading as a smaller bill.
    #[tokio::test]
    async fn a_summary_says_how_many_rows_it_could_not_price() {
        let store = store().await;
        store
            .record_usage(costing("acme", "alice", "anthropic", 810))
            .await
            .unwrap();
        let state = state_over_shared(store);
        let router = crate::routes::router(state.clone());

        // acme's two seeded rows carry no cost, and the one just written does.
        let sums = summary_of(&router, &state, "alice", "?tenant=acme").await;
        assert_eq!(sums["answered"], 3);
        assert_eq!(sums["cost_micros"], 810);
        assert_eq!(sums["unpriced"], 2);
    }

    /// What the summary exists for: how much of a window the local model carried, how
    /// much the arbiter was paid for, and how much nothing could decide. A tenant reads
    /// its own counts, an administrator reads anyone's, and nobody reads another
    /// tenant's — the same authorization as every other number on this router, asserted
    /// again because this is a new field on it.
    #[tokio::test]
    async fn the_summary_counts_what_each_level_decided() {
        let store = store().await;
        for (level, many) in [
            (Decider::Request, 1),
            (Decider::Turn, 4),
            (Decider::Classifier, 3),
            (Decider::Arbiter, 2),
        ] {
            for _ in 0..many {
                store
                    .record_usage(NewUsage {
                        dispatched_class: Some(Difficulty::Hard),
                        decided_by: Some(level),
                        ..spent("acme", "alice", "anthropic")
                    })
                    .await
                    .unwrap();
            }
        }
        let state = state_over_shared(store);
        let router = crate::routes::router(state.clone());

        let sums = summary_of(&router, &state, "alice", "?tenant=acme").await;
        let decided = &sums["decided"];
        assert_eq!(decided["request"], 1);
        assert_eq!(decided["turn"], 4);
        assert_eq!(decided["classifier"], 3, "the real local call rate");
        assert_eq!(decided["arbiter"], 2);
        assert_eq!(decided["undecided"], 2, "the two rows the fixture seeded");
        let counted: u64 = ["request", "turn", "classifier", "arbiter", "undecided"]
            .iter()
            .map(|level| decided[*level].as_u64().unwrap())
            .sum();
        assert_eq!(
            counted,
            sums["answered"].as_u64().unwrap(),
            "every answered request is decided by one level or by none"
        );

        // The administrator reads anyone's, and sees the other tenant's row besides.
        let all = summary_of(&router, &state, "root", "").await;
        assert_eq!(all["decided"]["classifier"], 3);
        assert_eq!(all["decided"]["undecided"], 3);

        // And a tenant still reads nobody else's.
        let refused = read_at(
            &router,
            &state,
            "alice",
            "/_ip/usage/summary",
            "?tenant=other",
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_window_narrows_the_answer() {
        let store = store().await;
        let state = state_over_shared(store);
        let router = crate::routes::router(state.clone());
        // The list and the summary open their windows the same way: a moment ahead of
        // every row leaves nothing, and the floor of the clock leaves everything.
        let future = Timestamp::now().unix_seconds() + 3_600;
        let sums = summary_of(&router, &state, "root", &format!("?after={future}")).await;
        assert_eq!(
            (sums["answered"].as_u64(), sums["sent"].as_u64()),
            (Some(0), Some(0))
        );
        assert_eq!(sums["since"], future);
    }

    #[tokio::test]
    async fn hedged_rows_cost_more_than_the_gateway_answered() {
        let store = store().await;
        store
            .record_usage(NewUsage {
                raced_across: Hedge::new(3).unwrap(),
                ..spent("acme", "alice", "anthropic")
            })
            .await
            .unwrap();
        store
            .record_usage(NewUsage {
                served: Served::Cache,
                served_by: None,
                ..spent("acme", "bob", "anthropic")
            })
            .await
            .unwrap();
        let state = state_over_shared(store);
        let router = crate::routes::router(state.clone());
        let sums = summary_of(&router, &state, "alice", "?tenant=acme").await;
        // Four answered — the cached one among them — and 1+1+3 sent past three rows,
        // the cached one paying nothing.
        assert_eq!(sums["answered"], 4);
        assert_eq!(sums["sent"], 5);
    }

    #[tokio::test]
    async fn a_store_that_cannot_answer_gives_a_500_not_a_wrong_number() {
        let state = state_over_shared(store().await);
        state.store_failure().answer_only(1);
        let router = crate::routes::router(state.clone());
        let failed = read(&router, &state, "alice", "?tenant=acme").await;
        assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn a_caller_that_proved_nothing_reads_nothing() {
        let state = state_over_shared(store().await);
        let router = crate::routes::router(state.clone());
        let refused = router
            .oneshot(request("GET", "/_ip/usage", "", None))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    }
}
