CREATE TABLE tenants (
    row_id     BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    id         TEXT   NOT NULL,
    key        BYTEA  NOT NULL,
    created_at BIGINT NOT NULL
);

CREATE UNIQUE INDEX tenants_id ON tenants (id);

CREATE TABLE users (
    row_id     BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    id         TEXT   NOT NULL,
    passphrase TEXT   NOT NULL,
    kind       TEXT   NOT NULL,
    created_at BIGINT NOT NULL
);

CREATE UNIQUE INDEX users_id ON users (id);

CREATE TABLE memberships (
    tenant_row_id BIGINT NOT NULL REFERENCES tenants (row_id) ON DELETE CASCADE,
    user_row_id   BIGINT NOT NULL REFERENCES users (row_id) ON DELETE CASCADE,
    standing      TEXT   NOT NULL,
    PRIMARY KEY (tenant_row_id, user_row_id)
);

CREATE INDEX memberships_user ON memberships (user_row_id);

CREATE TABLE grants (
    row_id        BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_row_id   BIGINT NOT NULL REFERENCES users (row_id) ON DELETE CASCADE,
    tenant_row_id BIGINT REFERENCES tenants (row_id) ON DELETE CASCADE,
    api_id        TEXT,
    capability    TEXT   NOT NULL
);

CREATE UNIQUE INDEX grants_scope ON grants (
    user_row_id,
    (COALESCE(tenant_row_id, 0)),
    (COALESCE(api_id, '')),
    capability
);
