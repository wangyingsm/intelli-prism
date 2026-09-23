-- What each request cost. The tenant, user and api are held by name rather than by key: the
-- row is written on the request path, where a lookup per request would cost a round trip, and
-- what a tenant spent stays readable after the tenant is deleted.
CREATE TABLE usage (
    row_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    trace_id      TEXT    NOT NULL,
    turn_id       TEXT,
    tenant_id     TEXT    NOT NULL,
    user_id       TEXT    NOT NULL,
    api_id        TEXT    NOT NULL,
    model         TEXT,
    input_tokens  INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    served        TEXT    NOT NULL,
    latency_ms    INTEGER NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE INDEX usage_newest ON usage (created_at DESC, row_id DESC);
CREATE INDEX usage_tenant ON usage (tenant_id, created_at DESC);
CREATE INDEX usage_trace ON usage (trace_id);
