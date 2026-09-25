-- What a tenant, one of its accounts, or one of its apis may spend over a stretch of clock.
-- A scope holds at most one limit per thing counted per period; the narrowest scope a request
-- falls inside is the one that decides.
CREATE TABLE limits (
    row_id        INTEGER PRIMARY KEY,
    tenant_row_id INTEGER NOT NULL REFERENCES tenants (row_id) ON DELETE CASCADE,
    user_row_id   INTEGER REFERENCES users (row_id) ON DELETE CASCADE,
    api_id        TEXT,
    counted       TEXT    NOT NULL,
    period        TEXT    NOT NULL,
    allowance     INTEGER NOT NULL CHECK (allowance > 0),
    created_at    INTEGER NOT NULL
);

CREATE UNIQUE INDEX limits_scope
    ON limits (tenant_row_id, IFNULL(user_row_id, 0), IFNULL(api_id, ''), counted, period);

CREATE INDEX limits_tenant ON limits (tenant_row_id);

-- Every node holds the limits in memory beside the routes and the plugin rules, so a change to
-- one moves the same revision on and is carried by the same reload.
CREATE TRIGGER limits_insert_moves_the_revision AFTER INSERT ON limits
BEGIN
    UPDATE rule_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER limits_update_moves_the_revision AFTER UPDATE ON limits
BEGIN
    UPDATE rule_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER limits_delete_moves_the_revision AFTER DELETE ON limits
BEGIN
    UPDATE rule_revision SET revision = revision + 1 WHERE id = 1;
END;
