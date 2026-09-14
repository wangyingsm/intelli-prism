CREATE TABLE plugins (
    row_id     BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    checksum   TEXT   NOT NULL,
    kind       TEXT   NOT NULL,
    wasm       BYTEA  NOT NULL,
    created_at BIGINT NOT NULL
);

CREATE UNIQUE INDEX plugins_checksum ON plugins (checksum);

CREATE TABLE plugin_rules (
    row_id        BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    plugin_row_id BIGINT NOT NULL REFERENCES plugins (row_id) ON DELETE NO ACTION,
    tenant_row_id BIGINT REFERENCES tenants (row_id) ON DELETE CASCADE,
    user_row_id   BIGINT REFERENCES users (row_id) ON DELETE CASCADE,
    api_id        TEXT,
    kind          TEXT   NOT NULL,
    position      BIGINT NOT NULL,
    CHECK (tenant_row_id IS NOT NULL OR (user_row_id IS NULL AND api_id IS NULL)),
    CHECK ((tenant_row_id IS NULL) = (position <= 63))
);

CREATE UNIQUE INDEX plugin_rules_order ON plugin_rules ((COALESCE(tenant_row_id, 0)), kind, position);

CREATE INDEX plugin_rules_tenant ON plugin_rules (tenant_row_id);
