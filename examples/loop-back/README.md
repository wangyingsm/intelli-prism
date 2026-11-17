# A graph that doubles back, end to end, against a real model

Two nodes and one loop back. The reviewer holds the drafter to a house style **the drafter was
never told**, so the first pass fails it; what makes the second pass succeed is the amendment the
reviewer wrote. That is the whole point of a loop back, and it is the thing no in-process test can
show you: every fake upstream in this repository answers whatever it is asked, so a model that
really judges its input is the only way to see the feature work.

```
draft ──> review ──┐
  ^                │  {"loop_back": "draft", "notes": "..."}
  └────────────────┘  at most twice
```

The loop back is **declared on the graph**, with its own cap. The reviewer decides whether to take
it; the graph decides whether it may, and how often.

## Running it

```
. ~/.config/secrets.env                 # or wherever the key lives
export IP_API_KEY="$DEEPSEEK_API_KEY"
examples/loop-back/run.sh               # writes out/stream.sse beside this file
```

`drsg` must be on the path, 2.28.0 or newer. Nothing else is needed: the server, its sqlite, its
sled and the plane the graph lives in are all made for the run and go when it ends.

Any OpenAI-compatible provider works. `IP_BASE_URL`, `IP_MODEL` and `IP_DIALECT` override the
DeepSeek defaults, `IP_INPUT` replaces the task, and `IP_PORT` / `IP_DRSG_PORT` move the two
listeners.

| File | What it is |
|---|---|
| `run.sh` | stands everything up, writes the graph, starts the run |
| `graph.json` | two nodes, one edge and one loop back, as `PUT /_ip/graphs/{id}` takes them |
| `agents/draft.json` | the drafter, which is told how to read `$again` |
| `agents/review.json` | the reviewer, which is **not** told how to ask for a loop back |
| `report.py` | reads an event stream and says what the loop did |
| `sample-run/` | one real run: the stream, each step's input and answer, the report |

## What one real run did

`sample-run/` is a run against `deepseek-chat` on 2026-11-18.

```
  draft   begins on     86 characters
  draft   answered  in   147/  520 tokens,   2479 characters out
  review  begins on   2509 characters
  review  answered  in   895/  157 tokens,    631 characters out
  * review sends the work back to draft, taken 1 of 2
    notes: The note breaks all three house-style rules. (1) There is no section headed exactly '## Risks...
  draft   begins on    740 characters  attempt 2, on what review said
  draft   answered  in   309/  449 tokens,   2171 characters out
  review  begins on   2201 characters
  review  answered  in   822/  449 tokens,   2171 characters out

  loops taken:   1
  answered with: review
  nodes, in the order they finished: draft -> review -> draft -> review
```

Read `sample-run/answers/3-draft-input.json` — it is the feature in one file:

```json
{
  "$again": {
    "review": {
      "attempt": 2,
      "notes": "The note breaks all three house-style rules. (1) There is no section headed exactly '## Risks'; add a section with exactly that heading. (2) ..."
    }
  },
  "$input": "Write a short technical note on why a write-through cache can still serve stale reads."
}
```

Three things are worth noticing in it.

**The drafter was given the question again.** `draft` is a node nothing feeds, so its ordinary
input is the run's own question; on a re-run the asker's notes arrive *beside* it under `$input`
rather than replacing it. A node that lost the question would be revising in the dark.

**The ask itself is not there.** The reviewer answered
`{"loop_back": "draft", "notes": "..."}`, and `loop_back` is addressed to the engine. What reaches
the drafter is everything else the reviewer said, keyed by the reviewer's name, with `attempt`
added — put in last, so a node cannot overwrite the count of its own attempts.

**The reviewer's template never mentions `loop_back`.** Read `agents/review.json`: it says what the
house style is and when to send work back, and nothing about how. The *how* — the json shape, the
steps it may name, each one's cap — is appended to its prompt by the run, derived from the graph, so
it cannot drift when a node is renamed or a cap changes. The last pass shows the other half of that
bargain: `review` answered with the note unchanged, 2171 characters in and 2171 out.

## Two things this example was tuned to avoid, both worth knowing

**A node told it may loop back will loop back, unless the pass case is as concrete as the fail
case.** An earlier reviewer prompt ended with "if it does not, send the work back"; the model then
answered `{"loop_back": "draft", "notes": "The note breaks no rules — it satisfies all three..."}`.
It had decided the note passed and sent it back anyway. The fix is in `agents/review.json` now: the
pass case is spelled out as an instruction of its own — answer with the note and nothing else, *no
json*, "however much better you could make it" — rather than left as the absence of a failure.

**Rules that contradict each other spend the cap.** The first version required the note to *end*
with a `## Risks` section and to have `CHECKED: 3` as its last line. The drafter could satisfy
neither reading of that, so three passes were made, the second ask was the cap's last, and the
third was refused:

```
  x review asked for draft and was refused (spent: its loop back has been taken as often as it may be)
```

That is the cap doing its job — a run that would otherwise not have terminated ended, with the last
draft standing as the answer — but it is also what an impossible rubric looks like from the outside.
`loop_refused` with `spent` in a stream means the loop ran out, not that anything broke.

## What it costs

Four model turns where a graph without a loop would take two, which is the trade a loop back is:
one more pass through the expensive part in exchange for an answer that was checked. The cap is what
bounds it — per loop back, so a cheap critique loop and an expensive re-survey get their own budgets
— and a graph with several of them is bounded by their sum rather than by any one number.

`cost_micros` is `0` in the report because this deployment prices no models; see `ROADMAP.md`.
