use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ip_agents::{AgentConfig, AgentId, AgentName, AgentTemplate, ExtraPrompt, Sandbox};
use ip_core::{Capability, CapabilityScope, TenantId, Timestamp};
use ip_storage::{AgentOwner, By, NewAgent, StoredAgent};

use axum::response::sse::{Event as SseEvent, Sse};
use futures_util::StreamExt;
use ip_core::Protocol;
use ip_run::{AsWhom, Pinned, Requested, RunError, RunId, RunSignal, Running, Target, Toolshed};
use tokio_stream::wrappers::UnboundedReceiverStream;

use super::{Paged, store_refusal};
use crate::error::Unopened;
use crate::manage::Manager;
use crate::opening::toolshed;
use crate::state::AppState;

/// A refusal, boxed so the ordinary answer is not paid for by every caller.
type Refusal = Box<Response<Body>>;

/// The agent templates an operator has stored, under the prefix the router nests it at.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/{agent}", get(read).put(put).delete(remove))
        .route("/{agent}/run", post(start))
        .route("/{agent}/run_with_prompt", post(start_told_more))
}

/// What the audit trail records a template written or removed as having come through.
const ENDPOINT: &str = "/_ip/agents";

/// One agent template, as the api reports it.
#[derive(Debug, serde::Serialize)]
pub struct AgentView {
    /// Which template this is.
    pub id: String,
    /// What it is called.
    pub name: String,
    /// Whose it is, absent where it belongs to the deployment rather than to a tenant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// The file tree its agent would see.
    pub sandbox: Sandbox,
    /// What its agent would run under.
    pub config: AgentConfig,
    /// When it was written, in seconds since the unix epoch.
    pub created_at: Timestamp,
}

impl From<StoredAgent> for AgentView {
    fn from(stored: StoredAgent) -> Self {
        Self {
            id: stored.template.id.to_string(),
            name: stored.template.name.to_string(),
            tenant: stored.owner.tenant().map(ToString::to_string),
            sandbox: stored.template.sandbox,
            config: stored.template.config,
            created_at: stored.created_at,
        }
    }
}

/// A template as the api takes it. Which one it is comes from the path, not from here.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Written {
    /// What it is called.
    name: AgentName,
    /// The file tree its agent would see; none at all when left out.
    #[serde(default)]
    sandbox: Sandbox,
    /// What its agent would run under.
    config: AgentConfig,
}

/// Whose templates a request is about.
#[derive(Debug, Default, serde::Deserialize)]
pub struct Owned {
    /// The tenant whose templates these are; the deployment's own when left out.
    tenant: Option<String>,
}

/// One owner's templates, newest first.
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
    match state.store().agents(&owner, page).await {
        Ok(held) => Json(held.into_iter().map(AgentView::from).collect::<Vec<_>>()).into_response(),
        Err(error) => store_refusal(error),
    }
}

/// One template, or a report that this owner stores none under the name.
async fn read(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
    Query(owned): Query<Owned>,
) -> Response<Body> {
    let id = match AgentId::new(&id) {
        Ok(id) => id,
        Err(error) => return *unprocessable(error),
    };
    let owner = match owner_of(&state, &manager, owned).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    match state.store().agent(&owner, &id).await {
        Ok(Some(held)) => Json(AgentView::from(held)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Stores a template under the name the path gives it, replacing whatever was there.
///
/// Nothing is told to reload: a template is read when something runs one, and nothing does.
async fn put(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
    Query(owned): Query<Owned>,
    body: Bytes,
) -> Response<Body> {
    let id = match AgentId::new(&id) {
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
    let writing = NewAgent {
        owner,
        template: AgentTemplate {
            id,
            name: written.name,
            sandbox: written.sandbox,
            config: written.config,
        },
    };
    let by = By::new(manager.user().clone(), ENDPOINT);
    match state.store().put_agent(writing, &by).await {
        // The template as stored, so the caller learns when it was written without asking again.
        Ok(stored) => (StatusCode::CREATED, Json(AgentView::from(stored))).into_response(),
        Err(error) => store_refusal(error),
    }
}

/// Removes a template, or reports that this owner stores none under the name.
async fn remove(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
    Query(owned): Query<Owned>,
) -> Response<Body> {
    let id = match AgentId::new(&id) {
        Ok(id) => id,
        Err(error) => return *unprocessable(error),
    };
    let owner = match owner_of(&state, &manager, owned).await {
        Ok(owner) => owner,
        Err(refusal) => return *refusal,
    };
    let by = By::new(manager.user().clone(), ENDPOINT);
    match state.store().remove_agent(&owner, &id, &by).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => store_refusal(error),
    }
}

/// What starting a run is asked for.
#[derive(Debug, serde::Deserialize)]
struct Start {
    /// What the agent is being asked to do.
    input: String,
    /// Which model, where the request names one rather than taking the template's.
    #[serde(flatten)]
    requested: Requested,
}

/// What starting a run with a prompt of the caller's own is asked for.
#[derive(Debug, serde::Deserialize)]
struct StartTold {
    /// What the agent is being asked to do.
    input: String,
    /// What this caller adds to the template's prompt, for this run and no other.
    extra_prompt: ExtraPrompt,
    /// Which model, where the request names one rather than taking the template's.
    #[serde(flatten)]
    requested: Requested,
}

/// What one run is started from, however the caller asked for it.
struct Starting {
    /// Which template, as the path spells it.
    id: String,
    /// Whose templates the request is about.
    owned: Owned,
    /// Which model, where the request names one.
    requested: Requested,
    /// What the agent is being asked to do.
    input: String,
    /// What the caller added to the template's prompt, where it added anything.
    told: Option<ExtraPrompt>,
}

/// Runs a stored template, streaming what it does as it does it.
async fn start(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
    Query(owned): Query<Owned>,
    Json(asked): Json<Start>,
) -> Response<Body> {
    begin(
        state,
        manager,
        Starting {
            id,
            owned,
            requested: asked.requested,
            input: asked.input,
            told: None,
        },
    )
    .await
}

/// The same, with context this caller holds and the stored template does not.
///
/// A route of its own rather than a field on the one above, so that a caller asking for something
/// the deployment does not carry is refused by the router rather than by a field being quietly
/// ignored — and so that a template's own run endpoint keeps the body it has always taken.
async fn start_told_more(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<String>,
    Query(owned): Query<Owned>,
    Json(asked): Json<StartTold>,
) -> Response<Body> {
    begin(
        state,
        manager,
        Starting {
            id,
            owned,
            requested: asked.requested,
            input: asked.input,
            told: Some(asked.extra_prompt),
        },
    )
    .await
}

/// Everything both routes do, which is all of it bar where the extra prompt came from.
///
/// Everything that can refuse a run is settled before the stream opens, so a caller gets a
/// status code rather than one event saying it went wrong: the template is read, the tools are
/// resolved against the registry, the servers are opened and the target is worked out, all
/// first. Once the stream is open the run has begun and every outcome, refusals included,
/// arrives as a `done` event.
async fn begin(state: AppState, manager: Manager, starting: Starting) -> Response<Body> {
    let settled = match settled(&state, &manager, starting).await {
        Ok(settled) => settled,
        Err(refusal) => return *refusal,
    };
    let Settled {
        id: run_id,
        target,
        shed,
        config,
        pinned,
        input,
    } = settled;

    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let gateway = state.shared_gateway();
    tokio::spawn(async move {
        ip_run::run(
            &Running {
                id: run_id,
                gateway: gateway.as_ref(),
                target: &target,
                shed: &shed,
                config: &config,
                pinned: &pinned,
                reporting: &sender,
                signals: &RunSignal::never(),
            },
            &input,
        )
        .await;
    });
    Sse::new(UnboundedReceiverStream::new(receiver).map(|event| {
        Ok::<_, std::convert::Infallible>(
            SseEvent::default()
                .event(event.name())
                .data(event.body().to_string()),
        )
    }))
    .into_response()
}

/// Everything one run needs, resolved while a status code can still be answered.
struct Settled {
    /// What this run is called.
    id: RunId,
    /// Which endpoint its turns go to.
    target: Target,
    /// Everything it may call, opened.
    shed: Toolshed,
    /// What it runs under, the caller's own prompt folded in already.
    config: AgentConfig,
    /// Which model was settled on, and how.
    pinned: Pinned,
    /// What it is being asked to do.
    input: String,
}

/// Settles everything that can refuse, so the stream only ever opens on a run that has begun.
async fn settled(
    state: &AppState,
    manager: &Manager,
    starting: Starting,
) -> Result<Settled, Refusal> {
    let Starting {
        id,
        owned,
        requested,
        input,
        told,
    } = starting;
    let id = AgentId::new(&id).map_err(unprocessable)?;
    // A run spends money, so it is charged to a tenant rather than to the deployment.
    let Some(tenant) = owned.tenant.clone() else {
        return Err(Box::new(
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "name the tenant this run is charged to",
            )
                .into_response(),
        ));
    };
    let tenant = TenantId::new(&tenant).map_err(unprocessable)?;
    let owner = owner_of(state, manager, owned).await?;
    let template = match state.store().agent(&owner, &id).await {
        Ok(Some(held)) => held.template,
        Ok(None) => return Err(Box::new(StatusCode::NOT_FOUND.into_response())),
        Err(error) => return Err(Box::new(store_refusal(error))),
    };
    let authority = match manager.authority(state.store().as_ref(), &tenant).await {
        Ok(Some(authority)) => authority,
        Ok(None) => return Err(Box::new(StatusCode::FORBIDDEN.into_response())),
        Err(error) => return Err(Box::new(store_refusal(error))),
    };

    let config = template.config.clone();
    // Folded in here, before anything is resolved from it, so that what the run holds is one
    // configuration rather than a stored one and a correction travelling beside it.
    let config = match told {
        Some(extra) => config.told_also(&extra).map_err(unprocessable)?,
        None => config,
    };
    let pinned = Pinned::resolve(&requested, &config).map_err(unprocessable)?;
    let target = match Target::aimed(
        state.upstreams(),
        &pinned,
        AsWhom {
            authority,
            listen: state.listen(),
            protocol: Protocol::Http,
        },
    ) {
        Ok(target) => target,
        // Nothing named a model, which is the request's own omission rather than the
        // deployment's shape, so it is answered as the request's own mistake.
        Err(error @ RunError::NoModelChosen { .. }) => {
            return Err(Box::new(
                (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
            ));
        }
        Err(error) => return Err(unprocessable(error)),
    };
    let shed = match toolshed(state, &owner, config.tool_set()).await {
        Ok(shed) => shed,
        Err(Unopened::Storage(error)) => return Err(Box::new(store_refusal(error))),
        Err(error) => return Err(unprocessable(error)),
    };
    let id = RunId::generate().map_err(|error| {
        tracing::error!(%error, "could not name a run");
        Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response())
    })?;

    Ok(Settled {
        id,
        target,
        shed,
        config,
        pinned,
        input,
    })
}

/// Whose templates this caller may manage, or what it is told when it may manage none.
///
/// The system administrator manages the deployment's own and may name any tenant. Anyone else
/// names the tenant it is asking about and must hold `SysAgent` inside it, which a tenant owner
/// does by standing — so a caller only ever reaches the templates of the tenant it named. Nobody
/// but the administrator writes a template the whole deployment holds.
///
/// `SysAgent` governs reading as well as writing: a template is operational configuration, and a
/// prompt naming how a tenant works is not something every member of it needs.
async fn owner_of(
    state: &AppState,
    manager: &Manager,
    owned: Owned,
) -> Result<AgentOwner, Refusal> {
    let named = owned
        .tenant
        .as_deref()
        .map(TenantId::new)
        .transpose()
        .map_err(unprocessable)?;

    let Some(named) = named else {
        return if manager.is_system_administrator() {
            Ok(AgentOwner::Global)
        } else {
            Err(Box::new(
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "name the tenant whose agent templates you manage",
                )
                    .into_response(),
            ))
        };
    };
    if manager.is_system_administrator() {
        return Ok(AgentOwner::Tenant(named));
    }
    let scope = CapabilityScope::Tenant {
        user: manager.user().clone(),
        tenant: named.clone(),
    };
    match manager
        .allows(state.store().as_ref(), Capability::SysAgent, &scope)
        .await
    {
        Ok(true) => Ok(AgentOwner::Tenant(named)),
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

    use ip_agents::{
        AgentConfigParts, AgentName, AgentRole, AgentTemplate, Entries, MaxTurns, ObjectLocation,
        PathSegment, Prompt, ReqTimeout, SandboxEntry, TimeLimit, ToolSet,
    };
    use ip_core::{Grant, TnKey, UserId};
    use ip_storage::{
        AccountKind, AgentStore, AuditFilter, Entity, GrantStore, Membership, MembershipStore,
        NewTenant, NewUser, Page, SqliteStore, Standing, TenantStore, UserStore,
    };
    use tower::ServiceExt;

    use super::super::harness::{body_of, cookie_of, hashed, request, state_over_shared};
    use super::*;

    fn user(raw: &str) -> UserId {
        UserId::new(raw).unwrap()
    }

    fn tenant(raw: &str) -> TenantId {
        TenantId::new(raw).unwrap()
    }

    fn by(actor: &str, through: &'static str) -> By {
        By::new(user(actor), through)
    }

    fn template(id: &str, name: &str) -> AgentTemplate {
        AgentTemplate {
            id: AgentId::new(id).unwrap(),
            name: AgentName::new(name).unwrap(),
            sandbox: Sandbox::new(Entries::from([(
                PathSegment::new("python").unwrap(),
                SandboxEntry::File(ObjectLocation::new("s3://mybucket/bin/python").unwrap()),
            )]))
            .unwrap(),
            config: AgentConfig::new(AgentConfigParts {
                role: AgentRole::System,
                prompt: Prompt::new("Be helpful.").unwrap(),
                tool_set: ToolSet::default(),
                max_turns: MaxTurns::new(4).unwrap(),
                time_limit: TimeLimit::new(60).unwrap(),
                req_timeout: ReqTimeout::new(10).unwrap(),
                best_effort: None,
                model: None,
                api: None,
            })
            .unwrap(),
        }
    }

    /// Two tenants, four accounts, and one template under each of the three owners.
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
            (AgentOwner::Global, "deployment", "The deployment's"),
            (AgentOwner::Tenant(tenant("acme")), "acme-triage", "Acme's"),
            (
                AgentOwner::Tenant(tenant("other")),
                "other-triage",
                "Other's",
            ),
        ] {
            store
                .put_agent(
                    NewAgent {
                        owner,
                        template: template(id, name),
                    },
                    &by("root", "/_ip/agents"),
                )
                .await
                .unwrap();
        }
        Arc::new(store)
    }

    async fn read(router: &Router, state: &AppState, caller: &str, uri: &str) -> Response<Body> {
        router
            .clone()
            .oneshot(request(
                "GET",
                uri,
                &cookie_of(state, &user(caller)).await,
                None,
            ))
            .await
            .unwrap()
    }

    /// The names a listing answered with, in the order it answered.
    async fn named(response: Response<Body>) -> Vec<String> {
        let body = body_of(response).await;
        serde_json::from_str::<Vec<serde_json::Value>>(&body)
            .unwrap_or_else(|error| panic!("{error}: {body}"))
            .into_iter()
            .map(|held| held["id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// A template as a caller writes one, without the id the path carries.
    fn written(name: &str) -> String {
        format!(
            r#"{{"name":"{name}","sandbox":{{"python":"s3://mybucket/bin/python"}},"config":{{"role":"system","prompt":"Be helpful.","max_turns":4,"time_limit":60,"req_timeout":10}}}}"#
        )
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

    async fn fixture() -> (Router, AppState) {
        let state = state_over_shared(store().await);
        (super::super::router(state.clone()), state)
    }

    #[tokio::test]
    async fn the_administrator_reads_what_the_deployment_holds() {
        let (router, state) = fixture().await;

        let response = read(&router, &state, "root", "/_ip/agents").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(named(response).await, ["deployment"]);
    }

    /// The rows, not the status code: a caller admitted and then handed too much is what the
    /// isolation cases cannot see.
    #[tokio::test]
    async fn an_owner_reads_only_the_templates_of_its_own_tenant() {
        let (router, state) = fixture().await;

        let response = read(&router, &state, "alice", "/_ip/agents?tenant=acme").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(named(response).await, ["acme-triage"]);
    }

    #[tokio::test]
    async fn the_administrator_may_name_a_tenant_and_read_its_templates() {
        let (router, state) = fixture().await;

        let response = read(&router, &state, "root", "/_ip/agents?tenant=other").await;

        assert_eq!(named(response).await, ["other-triage"]);
    }

    #[tokio::test]
    async fn a_member_holding_nothing_is_refused() {
        let (router, state) = fixture().await;

        let response = read(&router, &state, "bob", "/_ip/agents?tenant=acme").await;

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

        let response = read(&router, &state, "bob", "/_ip/agents?tenant=acme").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(named(response).await, ["acme-triage"]);
    }

    #[tokio::test]
    async fn an_owner_of_one_tenant_is_refused_another() {
        let (router, state) = fixture().await;

        let response = read(&router, &state, "carol", "/_ip/agents?tenant=acme").await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_caller_that_is_not_the_administrator_must_name_a_tenant() {
        let (router, state) = fixture().await;

        let response = read(&router, &state, "alice", "/_ip/agents").await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body_of(response).await.contains("name the tenant"),
            "the caller is told what to do about it"
        );
    }

    #[tokio::test]
    async fn one_template_is_read_by_the_name_it_was_written_under() {
        let (router, state) = fixture().await;

        let response = read(&router, &state, "root", "/_ip/agents/deployment").await;

        assert_eq!(response.status(), StatusCode::OK);
        let held: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
        assert_eq!(held["id"], "deployment");
        assert_eq!(held["name"], "The deployment's");
        assert_eq!(held["tenant"], serde_json::Value::Null);
        assert_eq!(held["sandbox"]["python"], "s3://mybucket/bin/python");
        assert_eq!(held["config"]["max_turns"], 4);
    }

    #[tokio::test]
    async fn a_tenants_template_says_whose_it_is() {
        let (router, state) = fixture().await;

        let response = read(
            &router,
            &state,
            "alice",
            "/_ip/agents/acme-triage?tenant=acme",
        )
        .await;

        let held: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
        assert_eq!(held["tenant"], "acme");
    }

    /// Not forbidden: from outside, a template another owner holds and one nobody wrote are the
    /// same thing, which is the rule every refusal in this api follows.
    #[tokio::test]
    async fn a_template_another_owner_holds_is_not_found() {
        let (router, state) = fixture().await;

        let theirs = read(&router, &state, "root", "/_ip/agents/acme-triage").await;
        let absent = read(&router, &state, "root", "/_ip/agents/nothing-here").await;

        assert_eq!(theirs.status(), StatusCode::NOT_FOUND);
        assert_eq!(absent.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_name_that_is_not_one_is_the_callers_own_mistake() {
        let (router, state) = fixture().await;

        let response = read(&router, &state, "root", "/_ip/agents/not%20an%20id").await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_tenant_nobody_created_holds_nothing_rather_than_the_deployments() {
        let (router, state) = fixture().await;

        let listed = read(&router, &state, "root", "/_ip/agents?tenant=nobody").await;
        let one = read(
            &router,
            &state,
            "root",
            "/_ip/agents/deployment?tenant=nobody",
        )
        .await;

        assert_eq!(listed.status(), StatusCode::NOT_FOUND);
        assert_eq!(one.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_written_template_is_read_back() {
        let (router, state) = fixture().await;

        let response = send(
            &router,
            &state,
            "PUT",
            "root",
            "/_ip/agents/triage",
            Some(&written("Triage")),
        )
        .await;

        assert_eq!(response.status(), StatusCode::CREATED);
        let said: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
        assert_eq!(said["id"], "triage");
        assert_eq!(said["name"], "Triage");
        assert!(
            said["created_at"].as_i64().unwrap() > 0,
            "the caller learns when it was written without asking again"
        );
        let held = read(&router, &state, "root", "/_ip/agents/triage").await;
        assert_eq!(held.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn writing_a_template_again_replaces_it() {
        let (router, state) = fixture().await;

        for name in ["First", "Second"] {
            let response = send(
                &router,
                &state,
                "PUT",
                "root",
                "/_ip/agents/triage",
                Some(&written(name)),
            )
            .await;
            assert_eq!(response.status(), StatusCode::CREATED);
        }

        let listed = read(&router, &state, "root", "/_ip/agents").await;
        assert_eq!(named(listed).await, ["triage", "deployment"]);
        let one = read(&router, &state, "root", "/_ip/agents/triage").await;
        let said: serde_json::Value = serde_json::from_str(&body_of(one).await).unwrap();
        assert_eq!(said["name"], "Second");
    }

    #[tokio::test]
    async fn an_owner_writes_and_removes_its_own_tenants() {
        let (router, state) = fixture().await;

        let written_at = send(
            &router,
            &state,
            "PUT",
            "alice",
            "/_ip/agents/triage?tenant=acme",
            Some(&written("Triage")),
        )
        .await;
        assert_eq!(written_at.status(), StatusCode::CREATED);
        let said: serde_json::Value = serde_json::from_str(&body_of(written_at).await).unwrap();
        assert_eq!(said["tenant"], "acme");

        let removed = send(
            &router,
            &state,
            "DELETE",
            "alice",
            "/_ip/agents/triage?tenant=acme",
            None,
        )
        .await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        let listed = read(&router, &state, "alice", "/_ip/agents?tenant=acme").await;
        assert_eq!(named(listed).await, ["acme-triage"]);
    }

    /// Refused, and refused without writing: a 403 answered after the row landed would be a
    /// report about the response rather than about the store.
    #[tokio::test]
    async fn a_member_holding_nothing_writes_nothing() {
        let (router, state) = fixture().await;

        let response = send(
            &router,
            &state,
            "PUT",
            "bob",
            "/_ip/agents/triage?tenant=acme",
            Some(&written("Triage")),
        )
        .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let held = state
            .store()
            .agents(&AgentOwner::Tenant(tenant("acme")), Page::default())
            .await
            .unwrap();
        assert_eq!(held.len(), 1, "only what the fixture wrote is there");
    }

    #[tokio::test]
    async fn nobody_but_the_administrator_writes_what_the_deployment_holds() {
        let (router, state) = fixture().await;

        let response = send(
            &router,
            &state,
            "PUT",
            "alice",
            "/_ip/agents/triage",
            Some(&written("Triage")),
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            state
                .store()
                .agent(&AgentOwner::Global, &AgentId::new("triage").unwrap())
                .await
                .unwrap()
                .is_none()
        );
    }

    /// The whole point of the types: a template that cannot run is refused at the door rather
    /// than stored and discovered later.
    #[tokio::test]
    async fn a_body_the_types_refuse_is_the_callers_own_mistake() {
        let (router, state) = fixture().await;
        let cases = [
            (
                r#"{"name":"Triage","config":{"role":"system","prompt":"Be helpful.","max_turns":4,"time_limit":10,"req_timeout":60}}"#,
                "outlive",
            ),
            (
                r#"{"name":" Triage","config":{"role":"system","prompt":"Be helpful.","max_turns":4,"time_limit":60,"req_timeout":10}}"#,
                "whitespace",
            ),
            (
                r#"{"name":"Triage","sandbox":{"..":"s3://b/x"},"config":{"role":"system","prompt":"Be helpful.","max_turns":4,"time_limit":60,"req_timeout":10}}"#,
                r#"may not name "..""#,
            ),
            (
                r#"{"name":"Triage","owner":"acme","config":{"role":"system","prompt":"Be helpful.","max_turns":4,"time_limit":60,"req_timeout":10}}"#,
                "owner",
            ),
        ];

        for (body, says) in cases {
            let response = send(
                &router,
                &state,
                "PUT",
                "root",
                "/_ip/agents/triage",
                Some(body),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{body}"
            );
            let said = body_of(response).await;
            assert!(said.contains(says), "{says} is not in {said}");
        }
    }

    #[tokio::test]
    async fn removing_a_template_that_is_absent_is_reported_missing() {
        let (router, state) = fixture().await;

        let absent = send(
            &router,
            &state,
            "DELETE",
            "root",
            "/_ip/agents/nothing",
            None,
        )
        .await;
        let theirs = send(
            &router,
            &state,
            "DELETE",
            "root",
            "/_ip/agents/acme-triage",
            None,
        )
        .await;

        assert_eq!(absent.status(), StatusCode::NOT_FOUND);
        assert_eq!(theirs.status(), StatusCode::NOT_FOUND);
        let held = read(&router, &state, "root", "/_ip/agents?tenant=acme").await;
        assert_eq!(
            named(held).await,
            ["acme-triage"],
            "another owner's template is untouched by a removal that named it"
        );
    }

    #[tokio::test]
    async fn a_name_in_the_path_that_is_not_one_is_refused_before_anything_is_written() {
        let (router, state) = fixture().await;

        for method in ["PUT", "DELETE"] {
            let response = send(
                &router,
                &state,
                method,
                "root",
                "/_ip/agents/not%20an%20id",
                Some(&written("Triage")),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{method}"
            );
        }
    }

    #[tokio::test]
    async fn a_write_is_recorded_against_the_endpoint_it_came_through() {
        let (router, state) = fixture().await;
        send(
            &router,
            &state,
            "PUT",
            "alice",
            "/_ip/agents/triage?tenant=acme",
            Some(&written("Triage")),
        )
        .await;

        let trail = state
            .store()
            .audits(
                &AuditFilter {
                    tenant: Some(tenant("acme")),
                    actor: Some(user("alice")),
                    entity: Some(Entity::Agent),
                },
                Page::default(),
            )
            .await
            .unwrap();

        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0].through, "/_ip/agents");
        assert_eq!(trail[0].entity_id, "triage");
        assert_eq!(trail[0].after.as_ref().unwrap()["name"], "Triage");
    }

    /// A deployment that actually has an upstream to send a run's turns to.
    const SERVING: &str = r#"
[server]
listen = "127.0.0.1:8080"

[storage]
backend = "sqlite"
path = "./unopened.db"

[cache]
backend = "sled"
path = "./unopened"

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"

[[upstream]]
id = "anthropic"
base_url = "https://api.example.com/v1"
api_key = "sk-deployment"
model = "claude-opus-5"
caller_speaks = "anthropic"
upstream_speaks = "anthropic"
"#;

    /// An upstream that answers every turn with the same finished message.
    struct Answering;

    #[async_trait::async_trait]
    impl ip_gateway::upstream::Upstream for Answering {
        async fn send(
            &self,
            _request: axum::http::Request<ip_gateway::body::GatewayBody>,
        ) -> Result<axum::http::Response<ip_gateway::body::GatewayBody>, ip_gateway::UpstreamError>
        {
            let body = serde_json::json!({
                "id": "msg_1",
                "model": "claude-opus-5",
                "role": "assistant",
                "type": "message",
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "four"}],
                "usage": {"input_tokens": 10, "output_tokens": 2},
            })
            .to_string();
            Ok(axum::http::Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(ip_gateway::from_bytes(body.into()))
                .unwrap())
        }
    }

    async fn running_fixture() -> (Router, AppState) {
        let state = super::super::harness::serving(store().await, SERVING, Arc::new(Answering));
        (super::super::router(state.clone()), state)
    }

    /// The same upstream, keeping what each turn was sent, so a test can read the prompt a run
    /// really went out under rather than the one it was configured with.
    struct Recording(Arc<std::sync::Mutex<Vec<serde_json::Value>>>);

    #[async_trait::async_trait]
    impl ip_gateway::upstream::Upstream for Recording {
        async fn send(
            &self,
            request: axum::http::Request<ip_gateway::body::GatewayBody>,
        ) -> Result<axum::http::Response<ip_gateway::body::GatewayBody>, ip_gateway::UpstreamError>
        {
            use http_body_util::BodyExt;
            let bytes = request.into_body().collect().await.unwrap().to_bytes();
            self.0
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&bytes).unwrap());
            Answering.send(stub_request()).await
        }
    }

    /// A request for `Answering` to answer, which it does not read.
    fn stub_request() -> axum::http::Request<ip_gateway::body::GatewayBody> {
        axum::http::Request::builder()
            .body(ip_gateway::from_bytes(Bytes::new()))
            .unwrap()
    }

    /// The fixture above, over an upstream that keeps what it was sent.
    async fn recording_fixture() -> (Router, AppState, Arc<std::sync::Mutex<Vec<serde_json::Value>>>)
    {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let state = super::super::harness::serving(
            store().await,
            SERVING,
            Arc::new(Recording(Arc::clone(&sent))),
        );
        (super::super::router(state.clone()), state, sent)
    }

    /// Stores a template under a tenant, overwriting whatever the fixture put there.
    async fn template_asking(state: &AppState, tools: &[&str]) {
        let mut stored = template("acme-triage", "Acme's");
        stored.config = AgentConfig::new(AgentConfigParts {
            role: AgentRole::System,
            prompt: Prompt::new("Be helpful.").unwrap(),
            tool_set: ToolSet::new(
                tools
                    .iter()
                    .map(|one| ip_agents::ToolName::new(one).unwrap())
                    .collect(),
            )
            .unwrap(),
            max_turns: MaxTurns::new(2).unwrap(),
            time_limit: TimeLimit::new(30).unwrap(),
            req_timeout: ReqTimeout::new(10).unwrap(),
            best_effort: None,
            model: None,
            api: None,
        })
        .unwrap();
        state
            .store()
            .put_agent(
                NewAgent {
                    owner: AgentOwner::Tenant(tenant("acme")),
                    template: stored,
                },
                &by("root", "/_ip/agents"),
            )
            .await
            .unwrap();
    }

    async fn start_run(
        router: &Router,
        state: &AppState,
        caller: &str,
        uri: &str,
    ) -> Response<Body> {
        send(
            router,
            state,
            "POST",
            caller,
            uri,
            Some(r#"{"input":"how many?"}"#),
        )
        .await
    }

    /// Ruling 3, end to end: what the caller added reaches the model, after the template's own
    /// instruction and separated from it, and the stored template is not touched by it.
    #[tokio::test]
    async fn a_run_told_more_sends_both_prompts_and_leaves_the_template_alone() {
        let (router, state, sent) = recording_fixture().await;
        template_asking(&state, &[]).await;

        let response = send(
            &router,
            &state,
            "POST",
            "alice",
            "/_ip/agents/acme-triage/run_with_prompt?tenant=acme",
            Some(r#"{"input":"how many?","extra_prompt":"The table has a checksum column."}"#),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(streamed(response).await, ["turn", "text", "done"]);

        let turns = sent.lock().unwrap().clone();
        assert_eq!(turns.len(), 1, "{turns:?}");
        assert_eq!(
            turns[0]["system"],
            "Be helpful.\n\nThe table has a checksum column."
        );

        let held = state
            .store()
            .agent(
                &AgentOwner::Tenant(tenant("acme")),
                &AgentId::new("acme-triage").unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            held.template.config.prompt().as_str(),
            "Be helpful.",
            "a run's own prompt was written back to the template"
        );
    }

    /// The ordinary route is unchanged by the one above, and names the field nowhere.
    #[tokio::test]
    async fn a_run_not_told_more_sends_the_templates_prompt_alone() {
        let (router, state, sent) = recording_fixture().await;
        template_asking(&state, &[]).await;

        let response = start_run(
            &router,
            &state,
            "alice",
            "/_ip/agents/acme-triage/run?tenant=acme",
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(streamed(response).await, ["turn", "text", "done"]);
        assert_eq!(sent.lock().unwrap()[0]["system"], "Be helpful.");
    }

    /// An extra prompt its own type refuses is the caller's mistake, and is answered before
    /// anything is read: the extractor never builds one.
    #[tokio::test]
    async fn an_extra_prompt_that_is_not_one_is_refused_before_the_run() {
        let (router, state) = running_fixture().await;

        for body in [
            r#"{"input":"how many?","extra_prompt":""}"#,
            r#"{"input":"how many?"}"#,
        ] {
            let response = send(
                &router,
                &state,
                "POST",
                "alice",
                "/_ip/agents/acme-triage/run_with_prompt?tenant=acme",
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

    /// Every `event:` line a stream carried, in the order it carried them.
    async fn streamed(response: Response<Body>) -> Vec<String> {
        body_of(response)
            .await
            .lines()
            .filter_map(|line| line.strip_prefix("event: ").map(str::to_owned))
            .collect()
    }

    #[tokio::test]
    async fn a_run_streams_what_it_did_and_ends_with_what_it_came_to() {
        let (router, state) = running_fixture().await;

        let response = start_run(
            &router,
            &state,
            "alice",
            "/_ip/agents/acme-triage/run?tenant=acme",
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .map(|value| value.starts_with("text/event-stream")),
            Some(true)
        );
        assert_eq!(streamed(response).await, ["turn", "text", "done"]);
    }

    /// The whole point of settling everything first: a refusal is a status code a client can
    /// read, not one event inside a stream that says it went wrong.
    #[tokio::test]
    async fn a_template_naming_a_tool_no_server_offers_is_refused_before_the_stream_opens() {
        let (router, state) = running_fixture().await;
        template_asking(&state, &["nowhere.sql"]).await;

        let response = start_run(
            &router,
            &state,
            "alice",
            "/_ip/agents/acme-triage/run?tenant=acme",
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body_of(response).await.contains("nowhere"));
    }

    /// A run spends money, so it is charged to a tenant even when the caller could manage
    /// the deployment's own templates without naming one.
    #[tokio::test]
    async fn a_run_that_names_no_tenant_is_refused() {
        let (router, state) = running_fixture().await;

        let response = start_run(&router, &state, "root", "/_ip/agents/deployment/run").await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body_of(response).await.contains("charged"));
    }

    #[tokio::test]
    async fn a_template_this_tenant_does_not_hold_is_not_found() {
        let (router, state) = running_fixture().await;

        let response = start_run(
            &router,
            &state,
            "alice",
            "/_ip/agents/other-triage/run?tenant=acme",
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_caller_outside_the_tenant_cannot_run_its_templates() {
        let (router, state) = running_fixture().await;

        let response = start_run(
            &router,
            &state,
            "carol",
            "/_ip/agents/acme-triage/run?tenant=acme",
        )
        .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// The administrator manages every tenant's templates but is a member of none, so it has
    /// no standing inside one to run as.
    #[tokio::test]
    async fn the_administrator_has_no_authority_inside_a_tenant_to_run_as() {
        let (router, state) = running_fixture().await;

        let response = start_run(
            &router,
            &state,
            "root",
            "/_ip/agents/acme-triage/run?tenant=acme",
        )
        .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// A deployment that configured no upstream has nowhere to send a turn, and says so
    /// rather than opening a stream that cannot carry one.
    #[tokio::test]
    async fn a_deployment_with_no_upstream_refuses_a_run() {
        let (router, state) = fixture().await;

        let response = start_run(
            &router,
            &state,
            "alice",
            "/_ip/agents/acme-triage/run?tenant=acme",
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body_of(response).await.contains("no configured upstream"));
    }

    /// The fixture template names no model, so this proves the chain's last link: the
    /// upstream's own configured model is what a run falls back to.
    #[tokio::test]
    async fn a_template_naming_no_model_runs_on_the_one_the_upstream_was_configured_with() {
        let (router, state) = running_fixture().await;

        let response = start_run(
            &router,
            &state,
            "alice",
            "/_ip/agents/acme-triage/run?tenant=acme",
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_of(response).await;
        assert!(body.contains("claude-opus-5"), "{body}");
    }

    /// A run with nothing anywhere to name a model is the request's own omission, which is
    /// what tells it apart from the refusals that are the deployment's shape.
    #[tokio::test]
    async fn a_run_with_no_model_anywhere_is_a_bad_request() {
        let state = super::super::harness::serving_without_models(
            store().await,
            SERVING,
            Arc::new(Answering),
        );
        let router = super::super::router(state.clone());

        let response = start_run(
            &router,
            &state,
            "alice",
            "/_ip/agents/acme-triage/run?tenant=acme",
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body_of(response).await.contains("model"));
    }

    #[tokio::test]
    async fn a_template_name_that_is_not_one_is_the_callers_mistake() {
        let (router, state) = running_fixture().await;

        let response = start_run(
            &router,
            &state,
            "alice",
            "/_ip/agents/NOT%20AN%20ID/run?tenant=acme",
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// Every store call a request makes carries its own "the store could not answer" arm,
    /// and a database that works reaches none of them. Failing each in turn reaches them all.
    ///
    /// The sweep starts at the first call of the whole request — the extractor's own read of
    /// the caller's standing, where there is one — and the count is asserted, so a handler
    /// that gains or loses a store call fails here rather than quietly leaving a new arm
    /// untested. Each refused request changes nothing, which is what lets one fixture serve
    /// a whole sweep.
    #[tokio::test]
    async fn a_store_that_cannot_answer_is_the_server_s_own_failure() {
        for (method, uri, body, calls) in [
            ("GET", "/_ip/agents?tenant=acme", None, 4),
            ("GET", "/_ip/agents/acme-triage?tenant=acme", None, 4),
            (
                "PUT",
                "/_ip/agents/acme-triage?tenant=acme",
                Some(r#"{"name":"Acme's","config":null}"#),
                3,
            ),
            ("DELETE", "/_ip/agents/acme-triage?tenant=acme", None, 4),
            (
                "POST",
                "/_ip/agents/acme-triage/run?tenant=acme",
                Some(r#"{"input":"how many?"}"#),
                // Eight since M30: the retrievers a run may address are read beside the mcps.
                8,
            ),
        ] {
            let (router, state) = running_fixture().await;
            let cookie = cookie_of(&state, &user("alice")).await;
            let mut refused = 0;

            loop {
                state.store_failure().answer_only(refused);
                let response = router
                    .clone()
                    .oneshot(request(method, uri, &cookie, body))
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
