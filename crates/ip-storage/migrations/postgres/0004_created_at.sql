-- Every list the api serves is newest first, so every listed row records when it was made.
-- Rows already there take the migration's time; the default then goes, so a write that
-- forgets the time fails rather than inventing one.
ALTER TABLE memberships ADD COLUMN created_at BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::BIGINT;
ALTER TABLE memberships ALTER COLUMN created_at DROP DEFAULT;
ALTER TABLE grants ADD COLUMN created_at BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::BIGINT;
ALTER TABLE grants ALTER COLUMN created_at DROP DEFAULT;
ALTER TABLE routes ADD COLUMN created_at BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::BIGINT;
ALTER TABLE routes ALTER COLUMN created_at DROP DEFAULT;
ALTER TABLE plugin_rules ADD COLUMN created_at BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::BIGINT;
ALTER TABLE plugin_rules ALTER COLUMN created_at DROP DEFAULT;
