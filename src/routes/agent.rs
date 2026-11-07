use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ip_agents::{AgentConfig, AgentId, Sandbox};
use ip_core::{Capability, CapabilityScope, TenantId, Timestamp};
use ip_storage::{AgentOwner, StoredAgent};

use super::{Paged, store_refusal};
use crate::manage::Manager;
use crate::state::AppState;

/// A refusal, boxed so the ordinary answer is not paid for by every caller.
type Refusal = Box<Response<Body>>;

/// The agent templates an operator has stored, under the prefix the router nests it at.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/{agent}", get(read))
}

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
        AccountKind, AgentStore, By, GrantStore, Membership, MembershipStore, NewAgent, NewTenant,
        NewUser, SqliteStore, Standing, TenantStore, UserStore,
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
                use_best_effort: false,
                best_effort_strategy: None,
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
}
