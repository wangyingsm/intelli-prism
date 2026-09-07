CREATE TABLE routes (
    row_id          INTEGER PRIMARY KEY,
    key_protocol    TEXT    NOT NULL,
    key_host        TEXT    NOT NULL,
    key_port        INTEGER NOT NULL,
    key_path        TEXT    NOT NULL,
    target_protocol TEXT    NOT NULL,
    target_host     TEXT    NOT NULL,
    target_port     INTEGER NOT NULL,
    target_path     TEXT    NOT NULL
);

CREATE UNIQUE INDEX routes_key ON routes (key_protocol, key_host, key_port, key_path);
