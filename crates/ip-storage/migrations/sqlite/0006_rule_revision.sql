-- Every node holds the routes and plugin rules in memory. Any change to either, a cascade from
-- a deleted tenant or user included, moves this one number on in the same transaction, so a
-- copy of the rules can be told stale without comparing them.
CREATE TABLE rule_revision (
    id       INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL
);

INSERT INTO rule_revision (id, revision) VALUES (1, 0);

CREATE TRIGGER routes_insert_moves_the_revision AFTER INSERT ON routes
BEGIN
    UPDATE rule_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER routes_update_moves_the_revision AFTER UPDATE ON routes
BEGIN
    UPDATE rule_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER routes_delete_moves_the_revision AFTER DELETE ON routes
BEGIN
    UPDATE rule_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER route_targets_insert_moves_the_revision AFTER INSERT ON route_targets
BEGIN
    UPDATE rule_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER route_targets_update_moves_the_revision AFTER UPDATE ON route_targets
BEGIN
    UPDATE rule_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER route_targets_delete_moves_the_revision AFTER DELETE ON route_targets
BEGIN
    UPDATE rule_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER plugin_rules_insert_moves_the_revision AFTER INSERT ON plugin_rules
BEGIN
    UPDATE rule_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER plugin_rules_update_moves_the_revision AFTER UPDATE ON plugin_rules
BEGIN
    UPDATE rule_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE TRIGGER plugin_rules_delete_moves_the_revision AFTER DELETE ON plugin_rules
BEGIN
    UPDATE rule_revision SET revision = revision + 1 WHERE id = 1;
END;
