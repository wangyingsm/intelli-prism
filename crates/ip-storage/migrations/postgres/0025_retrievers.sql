-- Something a tenant's agents may look a fact up in: a dr-strange plane or a qdrant collection.
-- It belongs to whoever described it -- one tenant, or the deployment itself when no tenant is
-- named -- the way a template and a stored plugin do, and unlike an MCP server, which is the
-- deployment's alone. A retriever holds a tenant's own documents, so one id may name a different
-- corpus in each tenant and every read filters on the owner.
--
-- `source`, `tools` and `meta` hold the json the types in `ip-retrieve` read and write. The kind
-- of backend is the tag inside `source` rather than a column beside it, because the kind and the
-- source cannot then disagree; nothing queries this table by kind.
--
-- None of the four carries a CHECK, and `max_hits` carries none either, for the reason the usage
-- columns carry none: a constraint here would refuse a value before the read path saw it, leaving
-- that refusal unreachable from a raw row and therefore untested. What a retriever may say is its
-- own type's question, and that is what the raw row tests ask.
--
-- `auth_kind` and `auth_key` together are what `ip-retrieve`'s `RetrieverAuth` reads: the key is
-- null where the kind requires none, and the pair is deliberately unconstrained for the same
-- reason.
--
-- `auth_key` holds the credential as the deployment's own database holds it: nothing here is
-- encrypted at rest, and whoever can read this table can read the key, exactly as with an MCP
-- server's token. That is a caveat stated plainly rather than a property this schema provides.
--
-- Deliberately no trigger moving the rule revision on, unlike routes, plugin rules, limits and
-- prices. No node holds these in memory: whatever runs a node reads this table when a tool it was
-- given names a retriever. Its absence is a decision, not an omission.
CREATE TABLE retrievers (
    row_id        BIGSERIAL PRIMARY KEY,
    tenant_row_id BIGINT REFERENCES tenants (row_id) ON DELETE CASCADE,
    retriever_id  TEXT   NOT NULL,
    name          TEXT   NOT NULL,
    endpoint      TEXT   NOT NULL,
    auth_kind     TEXT   NOT NULL,
    auth_key      TEXT,
    source        TEXT   NOT NULL,
    tools         TEXT   NOT NULL,
    meta          TEXT   NOT NULL,
    max_hits      BIGINT NOT NULL,
    created_at    BIGINT NOT NULL
);

-- One id per owner, the deployment counting as an owner of its own.
CREATE UNIQUE INDEX retrievers_owner_id ON retrievers ((COALESCE(tenant_row_id, 0)), retriever_id);

-- Newest first, within one owner, is the only order a list is read in.
CREATE INDEX retrievers_owner_created_at
    ON retrievers ((COALESCE(tenant_row_id, 0)), created_at DESC, row_id DESC);
