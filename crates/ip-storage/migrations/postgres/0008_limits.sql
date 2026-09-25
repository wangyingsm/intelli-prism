-- What a tenant, one of its accounts, or one of its apis may spend over a stretch of clock.
-- A scope holds at most one limit per thing counted per period; the narrowest scope a request
-- falls inside is the one that decides.
CREATE TABLE limits (
    row_id        BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_row_id BIGINT NOT NULL REFERENCES tenants (row_id) ON DELETE CASCADE,
    user_row_id   BIGINT REFERENCES users (row_id) ON DELETE CASCADE,
    api_id        TEXT,
    counted       TEXT   NOT NULL,
    period        TEXT   NOT NULL,
    allowance     BIGINT NOT NULL CHECK (allowance > 0),
    created_at    BIGINT NOT NULL
);

CREATE UNIQUE INDEX limits_scope ON limits
    (tenant_row_id, (COALESCE(user_row_id, 0)), (COALESCE(api_id, '')), counted, period);

CREATE INDEX limits_tenant ON limits (tenant_row_id);

-- Every node holds the limits in memory beside the routes and the plugin rules, so a change to
-- one moves the same revision on and is carried by the same reload.
CREATE TRIGGER limits_move_the_revision AFTER INSERT OR UPDATE OR DELETE ON limits
    FOR EACH STATEMENT EXECUTE FUNCTION move_the_rule_revision();
