-- The hardest difficulty class the endpoint serves, by name; every class below it too.
-- There is no name for serving anything, so an endpoint holding nothing serves every class,
-- which is what every endpoint stored before this does.
ALTER TABLE route_targets ADD COLUMN serves TEXT;
