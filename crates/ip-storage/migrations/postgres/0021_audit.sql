-- Who changed what. Written in the same transaction as the change itself, so a change
-- that landed without its record is not a state this table can reach, and a record
-- whose change rolled back is not either.
--
-- `before` is NULL where nothing was there, `after` where nothing is left, and `tenant`
-- where what changed belongs to the deployment rather than to anyone -- a price, a
-- route, a plugin rule. Neither payload holds a secret: what an entity looks like here
-- is declared apart from what it stores, so a column added to a table is absent from
-- this one until somebody puts it there on purpose.
--
-- Nothing sweeps this table. A trail with an expiry is not one.
CREATE TABLE audit (
    row_id     BIGSERIAL PRIMARY KEY,
    actor      TEXT   NOT NULL,
    through    TEXT   NOT NULL,
    entity     TEXT   NOT NULL,
    entity_id  TEXT   NOT NULL,
    tenant     TEXT,
    before     TEXT,
    after      TEXT,
    created_at BIGINT NOT NULL
);

-- Newest first is the only order this is ever read in.
CREATE INDEX audit_created_at ON audit (created_at DESC, row_id DESC);
