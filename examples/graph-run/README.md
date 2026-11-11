# A run graph, end to end, against a real model

A five-node graph that writes a merge sort in two languages, benchmarks each, and compares the
two. It exists because an in-process test cannot tell you whether a graph really runs: every fake
upstream in this repository answers in one frame, on any path, with whatever was asked. This
example talks to a provider that does none of those things, and keeps a plane on a real
`drsg serve`.

```
rust-impl ─┐                  rust-bench ─┐
           ├ generation 0                 ├─ compare   generation 2
go-impl ───┘                  go-bench ───┘  generation 1
```

The two authors run **at once**. Each bencher waits for its own author. The comparer waits for
both benchers and is handed their answers as a json object keyed by node name — the fan-in of
M25's ruling 2.

## Running it

```
. ~/.config/secrets.env                 # or wherever the key lives
export IP_API_KEY="$DEEPSEEK_API_KEY"
examples/graph-run/run.sh               # writes out/stream.sse beside this file
```

`drsg` must be on the path, 2.28.0 or newer — the graph's order comes from its `toposort`, which
earlier releases do not carry. Nothing else is needed: the server, its sqlite, its sled and the
plane the graph lives in are all made for the run and go when it ends, so no database or plane
anything else is using is touched.

Any OpenAI-compatible provider works. `IP_BASE_URL`, `IP_MODEL` and `IP_DIALECT` override the
DeepSeek defaults, `IP_INPUT` replaces the task, and `IP_PORT` / `IP_DRSG_PORT` move the two
listeners.

| File | What it is |
|---|---|
| `run.sh` | stands everything up, writes the graph, starts the run |
| `graph.json` | the five nodes and the four edges, as `PUT /_ip/graphs/{id}` takes them |
| `agents/*.json` | one template per node, as `PUT /_ip/agents/{id}` takes them |
| `report.py` | reads an event stream and says what the graph did |
| `sample-run/` | one real run of it: the stream, each node's answer, the report |

## What one real run did

`sample-run/` is a run against `deepseek-chat` on 2026-11-09. Status 200, 21 events, no warnings.

```
  rust-impl   begins on     88 characters of input
  go-impl     begins on     88 characters of input
  rust-impl   answered  in    60/  202 tokens,    708 characters out
  go-impl     answered  in    63/  320 tokens,    884 characters out
  rust-bench  begins on    750 characters of input
  go-bench    begins on   1072 characters of input
  go-bench    answered  in   512/  212 tokens,    588 characters out
  rust-bench  answered  in   305/  613 tokens,   2064 characters out
  compare     begins on   2840 characters of input
  compare     answered  in  1078/  594 tokens,   2630 characters out

  answered with: compare
  stopped at:    nothing — every node answered
  spent:         2018/1941 tokens, 0 micros
  run id:        f871bc2e36bff1a5addfa7e742c170c0
```

Three things in that are worth reading rather than skipping:

**The generations are real, not a sequence.** Both authors are announced before either finishes,
and in generation 1 **`go-bench` finished before `rust-bench`** although `rust-bench` was spawned
first. A graph that ran its nodes one after another could not produce that ordering.

**The spend is every node's summed**: 60+63+512+305+1078 input and 202+320+212+613+594 output are
exactly the totals in `done`. **The cost is 0 micros and the tokens are not**, because no price
row exists for the model. Unpriced is not free — it counts as nothing, which is M20's ruling and
an easy trap: a deployment that forgets to load prices reads a graph run as costless.

**One run id on all 21 events.** Each node still mints its own `RunId`, so a node's turns are a
run's turns; the graph run id is what ties them together.

## Two things the example deliberately shows going wrong

**The benchers cannot benchmark.** No MCP server offering a shell is registered here, so a node
can write a runnable benchmark and cannot run one. The comparer said so itself, unprompted — "No
benchmark has been executed yet; none of the input contains measured numbers" — which is the
behaviour its prompt asks for and worth keeping as the honest shape of the example. Wiring a tool
server that runs commands is what would close it.

**The comparer cannot see what it is comparing.** Its parents are the two *benchers*, so the
implementations never reach it: its answer says "Go: implementation not shown". That is the graph's
shape doing exactly what it says, and the lesson is for whoever draws the next one — a node that
compares two things needs edges from those things, not only from the step after them. Adding
`rust-impl → compare` and `go-impl → compare` fixes it, and the graph permits it.

## Running the benchmarks the graph could not

The two implementations in `sample-run/answers/` were taken verbatim, given identical
xorshift-generated data, and timed with the clone inside the timed region on both sides — the
correction the comparer itself asked for. On one laptop, 10k `i64`:

| | ns/op | allocations/op |
|---|---|---|
| the Go node's merge sort, in Go | 757,000–826,000 | 3 |
| the Rust node's merge sort, in Rust | 974,000–1,014,000 | **19,999** |
| the Go node's *algorithm*, in Rust | **579,838** | 2 |

The comparer expected Rust to win and it lost, for a reason the comparer named but did not follow
through: the Rust node wrote a top-down recursive sort that allocates a fresh vector at every
call — 2n−1 of them — while the Go node wrote a bottom-up sort over two buffers allocated once.
Give Rust the Go node's algorithm and it is the fastest of the three.

So the languages differ by about a quarter and the two nodes' *algorithm choices* differ by about
two thirds. Two nodes given the same task in the same graph can disagree about the thing that
actually decides the answer, and a comparer reading only their benchmarks will not see it.
