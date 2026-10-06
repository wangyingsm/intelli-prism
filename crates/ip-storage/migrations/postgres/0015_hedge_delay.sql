-- How long a raced request waits behind the rule before its next arm goes out, in
-- milliseconds. Zero sends all arms together, the way every rule written before this does.
ALTER TABLE routes ADD COLUMN hedge_delay BIGINT NOT NULL DEFAULT 0 CHECK (hedge_delay >= 0);
