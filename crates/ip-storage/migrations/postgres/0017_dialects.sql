-- Which provider family this route's callers send, and which its endpoints speak. A
-- rule naming neither, or naming one family on both sides, forwards bytes untouched,
-- which is what every rule written before this did and still does — so both columns are
-- nullable and no stored rule changes meaning. No CHECK lists the names: a fourth family
-- must not need a migration, and the read path refuses a name it does not carry.
ALTER TABLE routes ADD COLUMN caller_speaks TEXT;
ALTER TABLE routes ADD COLUMN upstream_speaks TEXT;
