# A loop back and a conditional branch in one graph, end to end, against a real model

Four nodes. `judge` chooses which step runs next; `dig` sends the work back to `judge`. Putting
both in one graph is the point, because together they reach a state neither reaches alone: **a
node that answered and was then passed over.**

```
                        ┌─ some item names nothing ──> dig ─┐
scope ──> judge ────────┤                                   │ loop back to judge, at most twice
           ^            └─ every item names it ──> estimate │
           └────────────────────────────────────────────────┘   (dig also feeds estimate)
```

`judge` takes the branch to `dig`. `dig` sends the work back, so `judge` runs again and chooses
again — and the second time it chooses `estimate`. `dig` is now a branch `judge` no longer names,
so it is skipped, **although it has already run and answered.** Its answer is dropped from what the
graph answers with, and `estimate` runs on `judge` alone even though the graph gives it two parents.

## Running it

```
. ~/.config/secrets.env                 # or wherever the key lives
export IP_API_KEY="$DEEPSEEK_API_KEY"
examples/loop-and-branch/run.sh         # writes out/{looped,straight}.sse beside this file
```

`drsg` must be on the path, 2.28.0 or newer. Nothing else is needed: the server, its sqlite, its
sled and the plane the graph lives in are all made for the run and go when it ends.

Any OpenAI-compatible provider works. `IP_BASE_URL`, `IP_MODEL` and `IP_DIALECT` override the
DeepSeek defaults, `IP_VAGUE` / `IP_SHARP` replace the two change requests, and `IP_PORT` /
`IP_DRSG_PORT` move the two listeners.

| File | What it is |
|---|---|
| `run.sh` | stands everything up, writes the graph, starts both runs |
| `graph.json` | four nodes, two conditional edges, two plain ones and one loop back with its cap |
| `agents/scope.json` | lists the work a request implies, at the level the request is pitched at |
| `agents/judge.json` | the node that chooses, and the node a loop back comes home to |
| `agents/dig.json` | the node that sends work back, and the one that gets dropped |
| `agents/estimate.json` | the node that runs on whichever parent delivered |
| `report.py` | reads a stream and says what the loops and the branches did |
| `sample-run/` | two real runs: each stream, each step's input and answer, each report |

## What two real runs did

`sample-run/` is two runs against `deepseek-chat` on 2026-11-18 — one graph, two change requests.
`sample-run/looped/report.txt` in full:

```
  scope     answered  in   117/   37 tokens,    166 characters out
  judge     begins on scope
  judge     answered  in   540/  137 tokens,    576 characters out
  > judge sends the work on to dig
  - judge passes over estimate
  dig       begins on judge
  dig       answered  in   764/   97 tokens,    443 characters out
  ^ dig sends the work back to judge, taken 1 of 2
  judge     begins on scope                      attempt 2, on what dig said
  judge     answered  in   638/  198 tokens,    890 characters out
  - judge passes over dig
  > judge sends the work on to estimate
  . dig does not run: nothing reached it — having already answered once
  estimate  begins on judge
  estimate  answered  in   356/  120 tokens,    516 characters out

  loops taken:      1
  branches taken:   dig, estimate
  branches passed:  estimate, dig
  nodes skipped:    dig
  answered with:    estimate
  ran but dropped:  dig
  nodes, in the order they finished: scope -> judge -> dig -> judge -> estimate
```

And `sample-run/straight/`, the same graph on a request already specific enough:
`scope -> judge -> estimate`, `dig` skipped, no loop taken, 961/302 tokens against 2415/589.

Four things in that are the reason this example exists.

**`judge` chose differently the second time.** It appears twice in `branches taken` and twice in
`branches passed`, with `dig` on one side the first time and the other side the second. A loop back
forgets what the node it returns to chose, precisely so that the node can decide afresh; keeping
the first choice would have meant a second attempt bound by what the first one did.

**`dig` answered and was then skipped.** `ran but dropped: dig` is the state only a loop plus a
branch produces. Its record of having run is kept — it ran, it cost tokens, and `GraphRan::nodes`
says so — but what it said is not the run's to answer with, because a later pass decided against
it. Had the engine gone on treating its answer as delivered, `dig` would have counted as a node
nothing read from and the graph would have answered with a step it had passed over.

**`estimate` ran on one parent where the graph gives it two.** Read
`sample-run/looped/5-estimate-input.json`: one key, `judge`. The graph also has `dig -> estimate`,
and `dig` was skipped, so that edge delivered nothing — a node runs on what arrived, with one key
fewer rather than an empty one. Which is why `agents/estimate.json` is told there may be only one
key and not to ask for what is not there.

**What `dig` found had to travel through `judge` to survive.** Since `dig`'s answer is dropped, the
rewritten scope reaches `estimate` only because `judge` restated it in its own answer — read the
`notes` in that same file and the three rewritten items are all there, in judge's words. A graph
where the dropped node's work was the only copy would have lost it. **Put what must survive into
the answer of the node that chose**, not only into the answer of the node that might be passed over.

## What this example was tuned to, over six real runs

Every round below was diagnosed from the stream and the per-step inputs, not guessed. The two M28
findings still hold; these are the ones this graph added.

**A node whose only parent is the node that judged it gets a paraphrase, not the source.** The first
round had `dig` fed by `judge` alone, and `dig` refused to work: *"I cannot rewrite faulted items
without the original wording of the scope ... the quotes are the judge's paraphrase."* It then used
the loop back to **ask for input** — three times, until the cap stopped it. The tempting fix, an
edge `scope -> dig`, is wrong and worth knowing why: a node runs when *any* edge into it delivered,
so one unconditional parent makes a node unskippable and would have destroyed the very behaviour
this example exists to show. The fix is in the template — "**that scope is the scope**, there is no
other copy and no step is going to send you one" — and in `judge` carrying the scope verbatim.

**A formatting example in a template teaches the shape of the answer, not just of the content.**
This one cost two rounds and is the most useful thing here. `dig`'s template said the rewritten
scope goes "in your notes, as a numbered list", and illustrated an item as `"1. Add an Export PDF
button to the BillingPage component"`. The model produced a flawless numbered list — as its whole
answer, with no `loop_back` in it, so no loop was taken at all. The example had taught the output
shape and the engine's advice lost. The template now shows the *whole* answer, json and all, with
`"<step>"` left as a placeholder so no graph detail is copied into a template that would then drift:

```
{"loop_back": "<step>", "notes": "1. Add an Export PDF button to the BillingPage component ..."}
```

So: for a node that loops or branches, an example in its template either shows the engine's json
around the content or shows no shape at all. Anything in between is an instruction that competes
with the advice for the same slot, and the example wins.

**A rewrite phrased as advice gets judged as the original.** Before that, `dig` wrote *"item 1
should name the page component, e.g. BillingPage"* — every name correct, phrased as commentary —
and `judge` restated and judged the old wording, three passes running. "Write the items, not about
them", with one of each shown, is what fixed it.

**An instruction to explain before answering produces an answer the engine cannot read.** `judge`
was told to "restate the whole scope ... before anything else you say", and on one pass it wrote
two paragraphs of reasoning and then the json. `names()` parses the whole answer as one value, so
it read no branch at all: both branches were passed over, and the run ended with `judge`'s answer
standing. That is ruling 4's safety net working — a node whose answer is not an object takes no
branch, and the run ends normally rather than hanging — but it fires **silently**: nothing in the
stream says the answer could not be read. `judge`'s template now says to write nothing outside the
one answer it sends.

## What it costs

The looped run spent 2415/589 tokens over five node turns; the straight one 961/302 over three.
That is the trade: a judgement, cheap, that either saves a pass or buys one. The cap is what bounds
it — per loop back, so a cheap critique loop and an expensive re-survey get their own budgets — and
the branch is what stops the saved pass from running anyway.

`cost_micros` is `0` in the reports because this deployment prices no models; see `ROADMAP.md`.
