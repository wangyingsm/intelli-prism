use ip_agents::{
    AgentConfig, AgentConfigParts, AgentId, AgentName, AgentRole, AgentTemplate,
    BestEffortStrategy, ChosenApi, ChosenModel, Entries, MaxTurns, ObjectLocation, PathSegment,
    Prompt, ReqTimeout, Sandbox, SandboxEntry, TimeLimit, ToolName, ToolSet,
};
use ip_core::TenantId;

use crate::agent::{AgentOwner, NewAgent};
use crate::audit::By;
use crate::error::{Entity, StorageError};
use crate::list::Page;
use crate::store::Backend;

/// The account every suite test writes as, through the endpoint that writes a template.
pub(crate) fn by() -> By {
    By::new(crate::suite::fixture::user_id(), "/_ip/agents")
}

/// The tenant that owns nothing anybody created.
fn absent_tenant() -> TenantId {
    TenantId::new("nobody").unwrap()
}

/// A template under `id`, called `name`, with nothing in its sandbox.
pub(crate) fn template(id: &str, name: &str) -> AgentTemplate {
    AgentTemplate {
        id: AgentId::new(id).unwrap(),
        name: AgentName::new(name).unwrap(),
        sandbox: Sandbox::default(),
        config: config("Be helpful.", 4),
    }
}

/// A configuration with one prompt and one count of turns.
fn config(prompt: &str, turns: u16) -> AgentConfig {
    AgentConfig::new(AgentConfigParts {
        role: AgentRole::System,
        prompt: Prompt::new(prompt).unwrap(),
        tool_set: ToolSet::default(),
        max_turns: MaxTurns::new(turns).unwrap(),
        time_limit: TimeLimit::new(60).unwrap(),
        req_timeout: ReqTimeout::new(10).unwrap(),
        best_effort: None,
        model: None,
        api: None,
    })
    .unwrap()
}

/// That template, about to be written for one owner.
pub(crate) fn written_by(owner: AgentOwner, id: &str, name: &str) -> NewAgent {
    NewAgent {
        owner,
        template: template(id, name),
    }
}

pub(crate) async fn a_template_round_trips(store: &impl Backend) {
    let written = store
        .put_agent(
            written_by(AgentOwner::Global, "triage", "Support triage"),
            &by(),
        )
        .await
        .unwrap();

    assert_eq!(written.template, template("triage", "Support triage"));
    assert_eq!(written.owner, AgentOwner::Global);

    let read = store
        .agent(&AgentOwner::Global, &AgentId::new("triage").unwrap())
        .await
        .unwrap();
    assert_eq!(read, Some(written));
}

pub(crate) async fn every_part_of_a_template_survives_the_database(store: &impl Backend) {
    let template = AgentTemplate {
        id: AgentId::new("full").unwrap(),
        name: AgentName::new("Everything at once").unwrap(),
        sandbox: Sandbox::new(Entries::from([
            (
                PathSegment::new("bin").unwrap(),
                SandboxEntry::Directory(Entries::from([(
                    PathSegment::new("python").unwrap(),
                    SandboxEntry::File(ObjectLocation::new("s3://mybucket/bin/python").unwrap()),
                )])),
            ),
            (
                PathSegment::new("data").unwrap(),
                SandboxEntry::Directory(Entries::new()),
            ),
        ]))
        .unwrap(),
        config: AgentConfig::new(AgentConfigParts {
            role: AgentRole::User,
            prompt: Prompt::new("Read the schema, then answer.").unwrap(),
            tool_set: ToolSet::new(vec![
                ToolName::new("python").unwrap(),
                ToolName::new("sql").unwrap(),
            ])
            .unwrap(),
            max_turns: MaxTurns::new(12).unwrap(),
            time_limit: TimeLimit::new(600).unwrap(),
            req_timeout: ReqTimeout::new(30).unwrap(),
            best_effort: Some(BestEffortStrategy::Aggregate),
            model: Some(ChosenModel::new("claude-sonnet-4-5").unwrap()),
            api: Some(ChosenApi::Anthropic),
        })
        .unwrap(),
    };

    store
        .put_agent(
            NewAgent {
                owner: AgentOwner::Global,
                template: template.clone(),
            },
            &by(),
        )
        .await
        .unwrap();

    let read = store
        .agent(&AgentOwner::Global, &template.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.template, template);
}

pub(crate) async fn one_id_names_a_different_template_under_each_owner(store: &impl Backend) {
    let (tenant, _) = crate::suite::fixture::tenant_with_user(store).await;
    let theirs = AgentOwner::Tenant(tenant.id.clone());

    store
        .put_agent(
            written_by(AgentOwner::Global, "triage", "The deployment's"),
            &by(),
        )
        .await
        .unwrap();
    store
        .put_agent(written_by(theirs.clone(), "triage", "The tenant's"), &by())
        .await
        .unwrap();

    let id = AgentId::new("triage").unwrap();
    let global = store
        .agent(&AgentOwner::Global, &id)
        .await
        .unwrap()
        .unwrap();
    let owned = store.agent(&theirs, &id).await.unwrap().unwrap();

    assert_eq!(global.template.name.as_str(), "The deployment's");
    assert_eq!(owned.template.name.as_str(), "The tenant's");
    assert_eq!(owned.owner, theirs);
}

pub(crate) async fn writing_a_template_again_replaces_it(store: &impl Backend) {
    store
        .put_agent(written_by(AgentOwner::Global, "triage", "First"), &by())
        .await
        .unwrap();
    store
        .put_agent(written_by(AgentOwner::Global, "triage", "Second"), &by())
        .await
        .unwrap();

    let held = store
        .agents(&AgentOwner::Global, Page::new(10, 0, None))
        .await
        .unwrap();

    assert_eq!(held.len(), 1, "one owner, one id, one template");
    assert_eq!(held[0].template.name.as_str(), "Second");
}

pub(crate) async fn removing_a_template_leaves_the_others(store: &impl Backend) {
    store
        .put_agent(written_by(AgentOwner::Global, "triage", "Triage"), &by())
        .await
        .unwrap();
    store
        .put_agent(
            written_by(AgentOwner::Global, "summarise", "Summarise"),
            &by(),
        )
        .await
        .unwrap();

    store
        .remove_agent(&AgentOwner::Global, &AgentId::new("triage").unwrap(), &by())
        .await
        .unwrap();

    let held = store
        .agents(&AgentOwner::Global, Page::new(10, 0, None))
        .await
        .unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].template.id.as_str(), "summarise");
}

pub(crate) async fn removing_a_template_that_is_absent_reports_it_missing(store: &impl Backend) {
    let error = store
        .remove_agent(&AgentOwner::Global, &AgentId::new("triage").unwrap(), &by())
        .await
        .unwrap_err();

    assert!(
        matches!(error, StorageError::NotFound { entity: Entity::Agent, ref id } if id == "triage"),
        "{error:?}"
    );
}

pub(crate) async fn removing_one_owners_template_leaves_the_other_owners(store: &impl Backend) {
    let (tenant, _) = crate::suite::fixture::tenant_with_user(store).await;
    let theirs = AgentOwner::Tenant(tenant.id.clone());
    let id = AgentId::new("triage").unwrap();

    store
        .put_agent(
            written_by(AgentOwner::Global, "triage", "The deployment's"),
            &by(),
        )
        .await
        .unwrap();
    store
        .put_agent(written_by(theirs.clone(), "triage", "The tenant's"), &by())
        .await
        .unwrap();

    store
        .remove_agent(&AgentOwner::Global, &id, &by())
        .await
        .unwrap();

    assert_eq!(store.agent(&AgentOwner::Global, &id).await.unwrap(), None);
    assert!(store.agent(&theirs, &id).await.unwrap().is_some());
}

pub(crate) async fn a_template_for_a_tenant_that_is_not_there_is_refused(store: &impl Backend) {
    let error = store
        .put_agent(
            written_by(AgentOwner::Tenant(absent_tenant()), "triage", "Triage"),
            &by(),
        )
        .await
        .unwrap_err();

    assert!(
        matches!(
            error,
            StorageError::NotFound {
                entity: Entity::Tenant,
                ..
            }
        ),
        "{error:?}"
    );
}

pub(crate) async fn a_read_for_a_tenant_that_is_not_there_answers_with_nothing_of_the_deployments(
    store: &impl Backend,
) {
    store
        .put_agent(
            written_by(AgentOwner::Global, "triage", "The deployment's"),
            &by(),
        )
        .await
        .unwrap();

    let absent = AgentOwner::Tenant(absent_tenant());
    let one = store
        .agent(&absent, &AgentId::new("triage").unwrap())
        .await
        .unwrap_err();
    let listed = store
        .agents(&absent, Page::new(10, 0, None))
        .await
        .unwrap_err();

    for error in [one, listed] {
        assert!(
            matches!(
                error,
                StorageError::NotFound {
                    entity: Entity::Tenant,
                    ..
                }
            ),
            "a tenant nobody created must not read as the deployment: {error:?}"
        );
    }
}

pub(crate) async fn templates_come_newest_first_a_page_at_a_time(store: &impl Backend) {
    for id in ["first", "second", "third"] {
        store
            .put_agent(written_by(AgentOwner::Global, id, "Named"), &by())
            .await
            .unwrap();
    }

    let newest = store
        .agents(&AgentOwner::Global, Page::new(2, 0, None))
        .await
        .unwrap();
    let rest = store
        .agents(&AgentOwner::Global, Page::new(2, 2, None))
        .await
        .unwrap();

    let named: Vec<&str> = newest
        .iter()
        .chain(rest.iter())
        .map(|held| held.template.id.as_str())
        .collect();
    assert_eq!(named, ["third", "second", "first"]);
    assert_eq!(newest.len(), 2);
    assert_eq!(rest.len(), 1);
}

pub(crate) async fn a_list_holds_only_the_owner_it_names(store: &impl Backend) {
    let (tenant, _) = crate::suite::fixture::tenant_with_user(store).await;
    let theirs = AgentOwner::Tenant(tenant.id.clone());

    store
        .put_agent(
            written_by(AgentOwner::Global, "ours", "The deployment's"),
            &by(),
        )
        .await
        .unwrap();
    store
        .put_agent(written_by(theirs.clone(), "theirs", "The tenant's"), &by())
        .await
        .unwrap();

    let global = store
        .agents(&AgentOwner::Global, Page::new(10, 0, None))
        .await
        .unwrap();
    let owned = store.agents(&theirs, Page::new(10, 0, None)).await.unwrap();

    assert_eq!(global.len(), 1);
    assert_eq!(global[0].template.id.as_str(), "ours");
    assert_eq!(owned.len(), 1);
    assert_eq!(owned[0].template.id.as_str(), "theirs");
}

pub(crate) async fn deleting_a_tenant_takes_its_templates_with_it(store: &impl Backend) {
    let (tenant, _) = crate::suite::fixture::tenant_with_user(store).await;
    let theirs = AgentOwner::Tenant(tenant.id.clone());

    store
        .put_agent(written_by(theirs.clone(), "triage", "The tenant's"), &by())
        .await
        .unwrap();
    store
        .put_agent(
            written_by(AgentOwner::Global, "triage", "The deployment's"),
            &by(),
        )
        .await
        .unwrap();

    store
        .delete_tenant(&tenant.id, &crate::suite::identity::tenants_by())
        .await
        .unwrap();

    let id = AgentId::new("triage").unwrap();
    assert!(
        store.agent(&theirs, &id).await.is_err(),
        "the tenant is gone, so asking for its templates names no tenant"
    );
    assert!(
        store
            .agent(&AgentOwner::Global, &id)
            .await
            .unwrap()
            .is_some(),
        "the deployment's own survives a tenant going"
    );
}
