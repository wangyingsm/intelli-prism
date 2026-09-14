CREATE TABLE routes (
    row_id       BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    api_id       TEXT   NOT NULL,
    key_protocol TEXT   NOT NULL,
    key_host     TEXT   NOT NULL,
    key_port     BIGINT NOT NULL,
    key_path     TEXT   NOT NULL
);

CREATE UNIQUE INDEX routes_key ON routes (key_protocol, key_host, key_port, key_path);

CREATE INDEX routes_api ON routes (api_id);

CREATE TABLE route_targets (
    row_id       BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    route_row_id BIGINT NOT NULL REFERENCES routes (row_id) ON DELETE CASCADE,
    position     BIGINT NOT NULL,
    protocol     TEXT   NOT NULL,
    host         TEXT   NOT NULL,
    port         BIGINT NOT NULL,
    path         TEXT   NOT NULL
);

CREATE UNIQUE INDEX route_targets_position ON route_targets (route_row_id, position);
