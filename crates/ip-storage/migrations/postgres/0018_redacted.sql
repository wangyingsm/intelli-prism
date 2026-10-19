-- What the filter took out of that request and its answer: how many, and the kinds as
-- the filter named them. Both NULL together on a row no filter looked at, which is not
-- the same as a row it looked at and found nothing in — that one carries 0 and ''.
-- Counts and kinds, never values: a ledger holding the secrets it found would have
-- moved the leak rather than closed it.
ALTER TABLE usage ADD COLUMN redacted BIGINT;
ALTER TABLE usage ADD COLUMN redacted_kinds TEXT;
