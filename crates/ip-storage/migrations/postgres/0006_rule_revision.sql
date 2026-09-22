-- Every node holds the routes and plugin rules in memory. Any change to either, a cascade from
-- a deleted tenant or user included, moves this one number on in the same transaction, so a
-- copy of the rules can be told stale without comparing them.
CREATE TABLE rule_revision (
    id       SMALLINT PRIMARY KEY CHECK (id = 1),
    revision BIGINT   NOT NULL
);

INSERT INTO rule_revision (id, revision) VALUES (1, 0);

CREATE FUNCTION move_the_rule_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE rule_revision SET revision = revision + 1 WHERE id = 1;
    RETURN NULL;
END
$$;

CREATE TRIGGER routes_move_the_revision AFTER INSERT OR UPDATE OR DELETE ON routes
    FOR EACH STATEMENT EXECUTE FUNCTION move_the_rule_revision();

CREATE TRIGGER route_targets_move_the_revision AFTER INSERT OR UPDATE OR DELETE ON route_targets
    FOR EACH STATEMENT EXECUTE FUNCTION move_the_rule_revision();

CREATE TRIGGER plugin_rules_move_the_revision AFTER INSERT OR UPDATE OR DELETE ON plugin_rules
    FOR EACH STATEMENT EXECUTE FUNCTION move_the_rule_revision();
