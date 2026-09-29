-- How many endpoints behind the rule one request was raced across, what actually happened.
-- A request recorded before this was raced across one endpoint, the way it always was.
ALTER TABLE usage ADD COLUMN raced_across BIGINT NOT NULL DEFAULT 1 CHECK (raced_across >= 1);
