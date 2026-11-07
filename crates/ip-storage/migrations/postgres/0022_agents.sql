-- An agent template: what a system agent would be before anything runs one. It belongs to
-- whoever wrote it -- one tenant, or the deployment itself when no tenant is named -- the way a
-- stored plugin does, so one id may name a different template in each tenant and the
-- deployment's own set stands apart from all of them.
--
-- `sandbox` and `config` hold the json the types in `ip-agents` read and write. They carry no
-- CHECK on purpose: a constraint here would refuse a value before the read path saw it, which
-- would leave that refusal unreachable from a raw row and therefore untested. What a template
-- may say is its own type's question, and that is what the raw row tests ask.
--
-- Deliberately no trigger moving the rule revision on, unlike routes, plugin rules, limits and
-- prices. No node holds templates in memory and none reloads them, because nothing runs one yet;
-- whatever starts a run reads this table when it does. Its absence is a decision, not an omission.
CREATE TABLE agents (
    row_id        BIGSERIAL PRIMARY KEY,
    tenant_row_id BIGINT REFERENCES tenants (row_id) ON DELETE CASCADE,
    agent_id      TEXT   NOT NULL,
    name          TEXT   NOT NULL,
    sandbox       TEXT   NOT NULL,
    config        TEXT   NOT NULL,
    created_at    BIGINT NOT NULL
);

-- One id per owner, the deployment counting as an owner of its own.
CREATE UNIQUE INDEX agents_owner_id ON agents ((COALESCE(tenant_row_id, 0)), agent_id);

-- Newest first, within one owner, is the only order a list is read in.
CREATE INDEX agents_owner_created_at
    ON agents ((COALESCE(tenant_row_id, 0)), created_at DESC, row_id DESC);
