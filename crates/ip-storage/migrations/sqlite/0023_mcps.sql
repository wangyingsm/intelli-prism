-- An MCP server an agent's tools are reached over. Deployment-wide and the system
-- administrator's alone, like the prices and unlike a template: `DESIGN.md` makes `MCPs` a page
-- of "system global MCP settings", so there is no tenant column here and one id names one server
-- for everybody.
--
-- `auth_kind` and `auth_token` together are what `ip-mcp`'s `McpAuth` reads: the token is null
-- where the kind requires none. They carry no CHECK pairing them on purpose, for the reason the
-- usage columns carry none -- a constraint would refuse the mismatch before the read path saw it,
-- leaving that refusal unreachable from a raw row and therefore untested. Which pairings are
-- legal is the type's question, and that is what the raw row tests ask.
--
-- `auth_token` holds the credential as the deployment's own database holds it: nothing here is
-- encrypted at rest, and whoever can read this table can read the token. That is a caveat the
-- roadmap states plainly rather than a property this schema provides.
--
-- Deliberately no trigger moving the rule revision on, unlike routes, plugin rules, limits and
-- prices. No node holds these in memory: whatever starts a run reads this table when it needs to
-- know which servers exist. Its absence is a decision, not an omission.
CREATE TABLE mcps (
    row_id     INTEGER PRIMARY KEY AUTOINCREMENT,
    mcp_id     TEXT    NOT NULL,
    name       TEXT    NOT NULL,
    endpoint   TEXT    NOT NULL,
    auth_kind  TEXT    NOT NULL,
    auth_token TEXT,
    created_at INTEGER NOT NULL
);

-- One row per id, deployment-wide.
CREATE UNIQUE INDEX mcps_id ON mcps (mcp_id);

-- Newest first is the only order a list is read in.
CREATE INDEX mcps_created_at ON mcps (created_at DESC, row_id DESC);
