# A graph that branches, end to end, against a real model

One triage node, three ways out, and a condition on each. The same graph is run three times, on
three incident reports that differ only in what the conditions ask about; each run takes a
different branch and skips the other two. That last part is the whole point: every fake upstream
in this repository answers whatever it is asked, so a model that really reads a condition is the
only way to see the feature work.

```
         ┌── users are still affected, or the cause is unknown ──> escalate
triage ──┼── the incident is over and its cause is understood ──> close
         └── still going on, but nobody is affected ───────────> shelve
```

The conditions are **on the graph**, written by whoever drew it. What the node is told about them
is derived from the graph by the run, so it cannot drift when an edge is renamed or a condition is
reworded. The node decides; the graph decides what it may decide between.

## Running it

```
. ~/.config/secrets.env                 # or wherever the key lives
export IP_API_KEY="$DEEPSEEK_API_KEY"
examples/branch/run.sh                  # writes out/{escalated,closed,shelved}.sse beside this file
```

`drsg` must be on the path, 2.28.0 or newer. Nothing else is needed: the server, its sqlite, its
sled and the plane the graph lives in are all made for the run and go when it ends.

Any OpenAI-compatible provider works. `IP_BASE_URL`, `IP_MODEL` and `IP_DIALECT` override the
DeepSeek defaults, `IP_ONGOING` / `IP_SETTLED` / `IP_QUIET` replace the three reports, and
`IP_PORT` / `IP_DRSG_PORT` move the two listeners.

| File | What it is |
|---|---|
| `run.sh` | stands everything up, writes the graph, starts the three runs |
| `graph.json` | four nodes and three conditional edges, as `PUT /_ip/graphs/{id}` takes them |
| `agents/triage.json` | the node that chooses, which is **not** told how to name a branch |
| `agents/escalate.json`, `close.json`, `shelve.json` | the three it chooses between |
| `report.py` | reads an event stream and says which branch was taken and which node was skipped |
| `sample-run/` | three real runs: each stream, each step's input and answer, each report |

`report.py <stream> --answers <dir>` is what wrote `sample-run/`, so it can be written again.

## What three real runs did

`sample-run/` is three runs against `deepseek-chat` on 2026-11-18, one graph, three reports.

```
  triage    answered  in   407/   84 tokens,    365 characters out
  > triage sends the work on to escalate
  - triage passes over close
  - triage passes over shelve
  . close does not run: nothing reached it
  . shelve does not run: nothing reached it
  escalate  answered  in   236/  152 tokens,    727 characters out

  triage    answered  in   418/   82 tokens,    329 characters out
  > triage sends the work on to close
  . escalate does not run: nothing reached it
  . shelve does not run: nothing reached it

  triage    answered  in   411/   79 tokens,    363 characters out
  > triage sends the work on to shelve
  . escalate does not run: nothing reached it
  . close does not run: nothing reached it
```

Four things in that are worth noticing.

**A branch not taken is a node that does not run.** `close` and `shelve` are in the graph, their
templates are stored, and the run never asks the provider about them. A `branch_not_taken` event
says the condition did not hold; a `node_skipped` event says the consequence. The two are separate
because a node can be skipped for a reason that is not a branch — nothing reached it because
*its* parent was skipped, which is how a skip carries down a graph.

**What the graph answers is the run's shape, not the graph's.** All three runs end
`answered with: <the one node that ran second>`. `triage` is not in the answer, because something
read what it said; `close` and `shelve` are not, because they never ran. The same graph answers
with a different node each time.

**The triage template never mentions branching.** Read `agents/triage.json`: it says what to judge
about an incident and that the next step needs a summary, and nothing about naming a step, the
json shape, or which steps exist. The *how* is appended to its prompt by the run, derived from the
edges — the shape, the three names, each one's condition.

**A branching node's answer reaches its target whole.** `sample-run/escalated/2-escalate-input.json`:

```json
{
  "triage": "{\"branch\": [\"escalate\"], \"notes\": \"Checkout has been returning 503 for roughly 40% of requests since 02:10 UTC, and this is ongoing. Users are affected: support has seven tickets open in the last twenty minutes. The cause is not understood...\"}"
}
```

The `branch` key is still in there. A loop back strips its own key before handing the asker's words
on, because it builds a new object to do that; a branch passes the answer as the node gave it, so
the three downstream templates are each told to read the summary out of it and ignore anything
about which steps run next. Worth knowing before writing a template for a node a branch feeds.

## What this example found, and the remedy that is in it

**A node told it may choose will choose.** The two-branch version of this graph — `escalate` and
`close`, no `shelve` — judged both of its reports correctly on the first run, with no tuning at
all, where M28's loop back took two rounds. The third report is where this example earned its keep.

The engine's advice ends: *"If no condition holds, name none of them: answer `{"branch": []}` and
nothing further runs, which is a complete answer and not a failure."* That is ruling 4 of the
milestone, and `run.rs` has a test proving the engine does exactly that.

Give a real model a report where neither of two conditions holds — the reindex job: still failing,
cause fully understood, nobody affected — and it names `escalate` anyway. Eight runs, four
wordings, `deepseek-chat` every time. Its own summary of the report is correct:

```json
{"branch": ["escalate"], "notes": "A background reindex job has been failing every ten minutes
since 08:00 UTC. The stated cause is that its service account lost permission during last night's
key rotation; the job is still failing now. No user-facing page reads that index yet, so no users
are affected and there have been no complaints."}
```

Which is the whole of the problem: **the judgement is right and the choice is wrong.** What was
tried and did not move it, in the order it was tried:

- Telling the template not to speculate about a cause the report does not state and not to treat a
  workaround as a fix. Probed with and without — it changes nothing for this model. It is kept
  because it says what the operator meant, not because it is load-bearing.
- A paragraph in the template spelling out the none case, in the middle of the prompt.
- The same paragraph last, imperative and exclusive — *"you must name no step at all: answer with
  the summary alone. That is the only right answer for it"* — which is exactly the shape of fix
  that worked for M28's loop back.
- The advice text's own enumeration. It reads "one of them, several, or all", which does not list
  *none* among the options; "all of them, several, one, or none at all", plus "do not name a step
  whose condition you have just judged does not hold", moved nothing either. It was reverted rather
  than shipped as a fix it is not.

**The remedy is in the graph, not in the prompt.** Declare a branch for the case you would
otherwise have wanted no branch for. `shelve` — "the incident is still going on, but nobody is
affected and its cause is understood" — is that case written down, and the model that had named
`escalate` for that report eight times names `shelve` for it, correctly, as `sample-run/shelved/`
shows. **A model asked which of these is far more reliable than a model asked whether any.**

So take `{"branch": []}` as the engine's answer to a graph whose conditions do not cover a report,
and as the safety net it certainly is — a node that answers prose, or names only steps that are not
there, ends the run with its own answer rather than hanging or failing. But do not design control
flow around a model choosing it. A graph whose conditions are exhaustive never needs it, and that
is the graph to write.

## What it costs

Two model turns per run, where a graph that ran every node would take four and would also pay for
two answers nobody reads. That is the trade a branch is: one judgement, cheap, to avoid work the
report does not call for. Three branches cost the same as two — a condition is text in a prompt the
node was going to be sent anyway, so what a branch costs is the node it runs, not the choosing.

`cost_micros` is `0` in the reports because this deployment prices no models; see `ROADMAP.md`.
