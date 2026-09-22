-- A plugin belongs to whoever stored it: a tenant, or the global chain when no tenant is named.
-- The wasm is stored once however many own it, and goes when the last of them lets it go.
CREATE TABLE plugin_owners (
    plugin_row_id BIGINT NOT NULL REFERENCES plugins (row_id) ON DELETE CASCADE,
    tenant_row_id BIGINT REFERENCES tenants (row_id) ON DELETE CASCADE,
    created_at    BIGINT NOT NULL
);

CREATE UNIQUE INDEX plugin_owners_owner ON plugin_owners ((COALESCE(tenant_row_id, 0)), plugin_row_id);

-- Plugins stored before owners were recorded belong to every chain that runs them, and to
-- the global chain when none does, so none becomes unreachable.
INSERT INTO plugin_owners (plugin_row_id, tenant_row_id, created_at)
    SELECT DISTINCT plugin_row_id, tenant_row_id, EXTRACT(EPOCH FROM now())::BIGINT
    FROM plugin_rules;

INSERT INTO plugin_owners (plugin_row_id, tenant_row_id, created_at)
    SELECT row_id, NULL, created_at FROM plugins
    WHERE row_id NOT IN (SELECT plugin_row_id FROM plugin_owners);
