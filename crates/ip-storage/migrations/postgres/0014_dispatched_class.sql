-- The difficulty class the request was dispatched at, by name. A request dispatched
-- without a class has none, which is not a name in the enum, and no CHECK lists the
-- names: adding a class later must not need a migration, and the read path refuses a
-- name it does not carry.
ALTER TABLE usage ADD COLUMN dispatched_class TEXT;
