-- Every list the api serves is newest first, so every listed row records when it was made.
-- Sqlite adds a column only with a constant default, so each write sets the real time itself.
ALTER TABLE memberships ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE grants ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE routes ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE plugin_rules ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;

UPDATE memberships SET created_at = CAST(strftime('%s', 'now') AS INTEGER);
UPDATE grants SET created_at = CAST(strftime('%s', 'now') AS INTEGER);
UPDATE routes SET created_at = CAST(strftime('%s', 'now') AS INTEGER);
UPDATE plugin_rules SET created_at = CAST(strftime('%s', 'now') AS INTEGER);
