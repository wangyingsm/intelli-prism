-- Which level of the margin named the class on that row: the request, the turn's
-- memory, the local classifier or the arbiter. A row dispatched without a class has no
-- decider, and no CHECK lists the names: adding a level later must not need a
-- migration, and the read path refuses a name it does not carry.
ALTER TABLE usage ADD COLUMN decided_by TEXT;
