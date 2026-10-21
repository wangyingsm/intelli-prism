-- What that request cost, in micros of the deployment's currency, as the prices in force
-- when it was answered said. NULL where nothing priced the model, which is not a cost of
-- nothing: a free model is priced at 0 and says so. No CHECK on the sign, so the read path
-- is what refuses a stored value a cost cannot be, the way every other column here works.
ALTER TABLE usage ADD COLUMN cost_micros INTEGER;
