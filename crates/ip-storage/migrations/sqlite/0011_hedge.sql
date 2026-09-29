-- How many endpoints behind a rule one request is raced across.
-- A rule written before this reaches one endpoint, the way it always did.
ALTER TABLE routes ADD COLUMN hedge INTEGER NOT NULL DEFAULT 1 CHECK (hedge >= 1);
