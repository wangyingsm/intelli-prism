-- Which endpoint behind the rule answered, for a rule standing behind several. Absent for a
-- request the cache answered, which reached no endpoint at all.
ALTER TABLE usage ADD COLUMN served_host TEXT;
ALTER TABLE usage ADD COLUMN served_port INTEGER;
