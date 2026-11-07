//! Agent templates as they are stored: whose each one is, and what the trail says it said.

use async_trait::async_trait;
use ip_agents::{AgentId, AgentTemplate};
use ip_core::{TenantId, Timestamp};

use crate::audit::By;
use crate::error::StorageError;
use crate::list::Page;

/// Whose an agent template is: the deployment's, or one tenant's.
///
/// One id may name a different template under each owner, so an owner is part of every lookup
/// rather than something checked afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOwner {
    /// The deployment's own.
    Global,
    /// One tenant's.
    Tenant(TenantId),
}

impl AgentOwner {
    /// The tenant it belongs to, and none where it belongs to the deployment.
    #[must_use]
    pub const fn tenant(&self) -> Option<&TenantId> {
        match self {
            Self::Global => None,
            Self::Tenant(tenant) => Some(tenant),
        }
    }
}

/// A template about to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAgent {
    /// Whose it is.
    pub owner: AgentOwner,
    /// What it says.
    pub template: AgentTemplate,
}

/// A template as it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAgent {
    /// Whose it is.
    pub owner: AgentOwner,
    /// What it says.
    pub template: AgentTemplate,
    /// When it was written.
    pub created_at: Timestamp,
}

impl crate::audit::Auditable for StoredAgent {
    fn entity(&self) -> crate::error::Entity {
        crate::error::Entity::Agent
    }

    fn entity_id(&self) -> String {
        self.template.id.as_str().to_owned()
    }

    fn tenant(&self) -> Option<TenantId> {
        self.owner.tenant().cloned()
    }

    /// Everything a template says, because all of it is the change somebody made.
    ///
    /// A template holds no secret to leave out: a prompt, a tool name and an object location are
    /// what the operator wrote, and nothing here is a credential or a key.
    fn audited(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.template.name.as_str(),
            "owner": match &self.owner {
                AgentOwner::Global => "global".to_owned(),
                AgentOwner::Tenant(tenant) => tenant.as_str().to_owned(),
            },
            "sandbox": self.template.sandbox,
            "config": self.template.config,
        })
    }
}

/// Writes and reads the agent templates an operator has stored.
///
/// Nothing holds these in memory and no node is told to reload them, unlike a routing rule, a
/// limit or a price: nothing runs an agent yet, so whatever starts a run will read this when it
/// does.
#[async_trait]
pub trait AgentStore: Send + Sync {
    /// Writes a template, replacing whatever that owner stored under the same id.
    async fn put_agent(&self, agent: NewAgent, by: &By) -> Result<StoredAgent, StorageError>;

    /// Removes one owner's template, or reports it missing.
    async fn remove_agent(
        &self,
        owner: &AgentOwner,
        id: &AgentId,
        by: &By,
    ) -> Result<(), StorageError>;

    /// One owner's template, or nothing where that owner has none under the id.
    async fn agent(
        &self,
        owner: &AgentOwner,
        id: &AgentId,
    ) -> Result<Option<StoredAgent>, StorageError>;

    /// One owner's templates, newest first.
    ///
    /// An owner sees its own and no one else's: the deployment's set is not shown to a tenant, and
    /// a tenant's is not shown to the deployment, because a listing that mixed them would make one
    /// tenant's template look like something every tenant could initialize.
    async fn agents(
        &self,
        owner: &AgentOwner,
        page: Page,
    ) -> Result<Vec<StoredAgent>, StorageError>;
}

#[cfg(test)]
mod tests {
    use ip_agents::{
        AgentConfig, AgentConfigParts, AgentName, AgentRole, Entries, MaxTurns, ObjectLocation,
        PathSegment, Prompt, ReqTimeout, Sandbox, SandboxEntry, TimeLimit, ToolName, ToolSet,
    };

    use ip_core::UserId;

    use super::*;
    use crate::audit::{Auditable, NewAudit};
    use crate::error::Entity;

    fn template() -> AgentTemplate {
        AgentTemplate {
            id: AgentId::new("triage").unwrap(),
            name: AgentName::new("Support triage").unwrap(),
            sandbox: Sandbox::new(Entries::from([(
                PathSegment::new("python").unwrap(),
                SandboxEntry::File(ObjectLocation::new("s3://mybucket/bin/python").unwrap()),
            )]))
            .unwrap(),
            config: AgentConfig::new(AgentConfigParts {
                role: AgentRole::System,
                prompt: Prompt::new("Be helpful.").unwrap(),
                tool_set: ToolSet::new(vec![ToolName::new("python").unwrap()]).unwrap(),
                max_turns: MaxTurns::new(4).unwrap(),
                time_limit: TimeLimit::new(30).unwrap(),
                req_timeout: ReqTimeout::new(10).unwrap(),
                use_best_effort: false,
                best_effort_strategy: None,
                model: None,
                api: None,
            })
            .unwrap(),
        }
    }

    fn stored(owner: AgentOwner) -> StoredAgent {
        StoredAgent {
            owner,
            template: template(),
            created_at: Timestamp::from_unix_seconds(1_700_000_000).unwrap(),
        }
    }

    fn tenant() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    #[test]
    fn the_deployments_own_template_belongs_to_no_tenant() {
        assert_eq!(AgentOwner::Global.tenant(), None);
        assert_eq!(stored(AgentOwner::Global).tenant(), None);
    }

    #[test]
    fn a_tenants_template_names_the_tenant_it_is_recorded_against() {
        let owner = AgentOwner::Tenant(tenant());
        assert_eq!(owner.tenant(), Some(&tenant()));
        assert_eq!(stored(owner).tenant(), Some(tenant()));
    }

    #[test]
    fn a_recorded_template_names_itself_by_the_id_an_operator_spelled() {
        let agent = stored(AgentOwner::Global);
        assert_eq!(agent.entity(), Entity::Agent);
        assert_eq!(agent.entity_id(), "triage");
    }

    #[test]
    fn the_trail_carries_everything_a_template_says() {
        let recorded = stored(AgentOwner::Tenant(tenant())).audited();
        assert_eq!(recorded["name"], "Support triage");
        assert_eq!(recorded["owner"], "acme");
        assert_eq!(recorded["sandbox"]["python"], "s3://mybucket/bin/python");
        assert_eq!(recorded["config"]["prompt"], "Be helpful.");
        assert_eq!(recorded["config"]["role"], "system");
        assert_eq!(recorded["config"]["tool_set"][0], "python");
        assert_eq!(recorded["config"]["max_turns"], 4);
    }

    #[test]
    fn the_trail_says_global_where_a_template_is_the_deployments() {
        assert_eq!(stored(AgentOwner::Global).audited()["owner"], "global");
    }

    #[test]
    fn a_recorded_change_carries_the_template_on_each_side_it_has_one() {
        let by = By::new(UserId::new("alice").unwrap(), "/_ip/agents");
        let agent = stored(AgentOwner::Global);

        let created = NewAudit::created(by.clone(), &agent);
        assert_eq!(created.entity, Entity::Agent);
        assert_eq!(created.entity_id, "triage");
        assert_eq!(created.before, None);
        assert_eq!(created.after, Some(agent.audited()));

        let removed = NewAudit::removed(by, &agent);
        assert_eq!(removed.before, Some(agent.audited()));
        assert_eq!(removed.after, None);
    }
}
