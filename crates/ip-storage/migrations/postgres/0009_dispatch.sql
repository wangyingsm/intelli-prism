-- How a rule spreads its requests over the endpoints behind it, and what share each takes.
-- A share of nothing drains that endpoint: no strategy sends it a request.
-- A rule written before this dispatches the way it always did, to the first endpoint in turn.
ALTER TABLE routes ADD COLUMN strategy TEXT NOT NULL DEFAULT 'round_robin';
ALTER TABLE route_targets ADD COLUMN weight BIGINT NOT NULL DEFAULT 1 CHECK (weight >= 0);
