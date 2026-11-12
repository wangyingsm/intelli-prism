-- Two keys in `agents.config` become one, because two of them could contradict each other.
--
-- A template said whether to ask every upstream in `use_best_effort` and how to pick an answer
-- in `best_effort_strategy`, which is four combinations for three legal states -- the fourth, a
-- strategy beside a flag that was off, had to be refused by hand on every read. `best_effort`
-- carries all three: absent is one upstream, `auto` is every upstream with the picking left to
-- the deployment, and `aggr` or `one` is every upstream picked that way.
--
-- `auto` is therefore what the flag alone used to mean, and a row that had the flag off keeps
-- neither key. Nothing has ever read these -- the best-effort endpoint is unbuilt -- so this
-- moves configuration written ahead of its own feature and no behaviour.
--
-- The column is TEXT rather than jsonb, so the surgery casts both ways. Coming back through
-- jsonb normalises what it rewrites -- key order and spacing are the cast's, not serde's --
-- which the read path cannot see, since it parses by name. `->` is used in place of `?` so that
-- nothing in the statement looks like a bind placeholder to whatever runs it.
UPDATE agents
SET config = (
        (
            CASE
                WHEN (config::jsonb -> 'use_best_effort') = 'true'::jsonb THEN
                    config::jsonb || jsonb_build_object(
                        'best_effort',
                        COALESCE(config::jsonb -> 'best_effort_strategy', '"auto"'::jsonb)
                    )
                ELSE config::jsonb
            END
        ) - 'use_best_effort' - 'best_effort_strategy'
    )::text
WHERE (config::jsonb -> 'use_best_effort') IS NOT NULL
   OR (config::jsonb -> 'best_effort_strategy') IS NOT NULL;
