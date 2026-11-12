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
-- `config` is json because `ip-agents` writes it, which is the same reason the table carries no
-- CHECK on it: what a template may say is its own type's question. A row that is not json would
-- fail here rather than be skipped, which is the louder of the two wrong answers.
UPDATE agents
SET config = json_remove(
        CASE
            WHEN json_extract(config, '$.use_best_effort') IN (1, 'true') THEN
                json_set(
                    config,
                    '$.best_effort',
                    COALESCE(json_extract(config, '$.best_effort_strategy'), 'auto')
                )
            ELSE config
        END,
        '$.use_best_effort',
        '$.best_effort_strategy'
    )
WHERE json_extract(config, '$.use_best_effort') IS NOT NULL
   OR json_extract(config, '$.best_effort_strategy') IS NOT NULL;
