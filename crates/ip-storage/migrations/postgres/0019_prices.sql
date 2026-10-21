-- What a model costs, in micros of the deployment's currency per million tokens. Keyed by
-- model because that is what a usage row records, including when an answer echoes a model the
-- caller did not ask for, and because several upstreams serving one model are priced once.
--
-- Whole numbers, never a float: a bill summed from a million fractions has to come out the
-- same on two machines. The currency itself is the deployment's own and is not stored here;
-- the gateway converts nothing, so mixing two in one column would produce a meaningless total.
CREATE TABLE prices (
    row_id     BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    model      TEXT   NOT NULL,
    input      BIGINT NOT NULL CHECK (input >= 0),
    output     BIGINT NOT NULL CHECK (output >= 0),
    created_at BIGINT NOT NULL
);

CREATE UNIQUE INDEX prices_model ON prices (model);

-- Every node holds the prices in memory beside the routes and the limits, so a change to one
-- moves the same revision on and is carried by the same reload.
CREATE TRIGGER prices_move_the_revision AFTER INSERT OR UPDATE OR DELETE ON prices
    FOR EACH STATEMENT EXECUTE FUNCTION move_the_rule_revision();
