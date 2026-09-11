CREATE TABLE plugins (
    row_id     INTEGER PRIMARY KEY,
    checksum   TEXT    NOT NULL,
    kind       TEXT    NOT NULL,
    wasm       BLOB    NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX plugins_checksum ON plugins (checksum);

-- NO ACTION rather than RESTRICT: sqlite raises RESTRICT as a trigger error, which the driver
-- cannot tell apart from any other constraint, so a plugin in use would not read as in use.
CREATE TABLE plugin_rules (
    row_id        INTEGER PRIMARY KEY,
    plugin_row_id INTEGER NOT NULL REFERENCES plugins (row_id) ON DELETE NO ACTION,
    tenant_row_id INTEGER REFERENCES tenants (row_id) ON DELETE CASCADE,
    user_row_id   INTEGER REFERENCES users (row_id) ON DELETE CASCADE,
    api_id        TEXT,
    kind          TEXT    NOT NULL,
    position      INTEGER NOT NULL,
    CHECK (tenant_row_id IS NOT NULL OR (user_row_id IS NULL AND api_id IS NULL)),
    CHECK ((tenant_row_id IS NULL) = (position <= 63))
);

CREATE UNIQUE INDEX plugin_rules_order ON plugin_rules (IFNULL(tenant_row_id, 0), kind, position);

CREATE INDEX plugin_rules_tenant ON plugin_rules (tenant_row_id);
