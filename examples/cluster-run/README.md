# A run graph across a cluster, end to end, against a real model

Three gateways sharing one store, one cache and one plane, and a four-node graph started on one
of them. It exists because the half of M27 that matters cannot be shown in one process: a
dispatch, the record of where a node is running, the events coming home, and the answer left
behind where another coordinator could find it. `examples/graph-run` is the same system with one
member — read that one first if a graph itself is the question.

```
storage ─┐
cache ───┼─ generation 0        editor   generation 1
limits ──┘
```

The three scouts run **at once**, and the editor is handed their three answers as a json object
keyed by node name. The prompts are one sentence each on purpose: this example is about where a
node runs, not about what a model can write, and a graph that costs almost nothing is one people
actually run.

## Running it

```
. ~/.config/secrets.env                 # or wherever the key lives
export IP_API_KEY="$DEEPSEEK_API_KEY"
examples/cluster-run/run.sh             # writes out/ beside this file
```

Needs `drsg` on the path (2.28.0 or newer, for `toposort`), and **a postgres and a redis to point
at** — a cluster is machines that share a store and a cache, so there is nothing to demonstrate
without them. The defaults are the containers this repository's test gate uses; set
`IP_POSTGRES_URL` and `IP_REDIS_URL` for others. The run creates one database of its own and drops
it on the way out, and touches nothing else in either server.

Everything else — the three gateways, the plane, the schema, the cluster secret — is made for the
run and goes when it ends. The secret is drawn from `/dev/urandom` per run and written only into
files this user can read, as the api key is.

Any OpenAI-compatible provider works. `IP_BASE_URL`, `IP_MODEL` and `IP_DIALECT` override the
DeepSeek defaults, `IP_INPUT` replaces the question, and `IP_ALPHA_PORT`, `IP_BETA_PORT`,
`IP_GAMMA_PORT` and `IP_DRSG_PORT` move the four listeners.

| File | What it is |
|---|---|
| `run.sh` | stands up three members, writes the graph, starts the run, reads back what happened |
| `graph.json` | the four nodes and the three edges, as `PUT /_ip/graphs/{id}` takes them |
| `agents/*.json` | one template per node, as `PUT /_ip/agents/{id}` takes them |
| `report.py` | joins the event stream to the placement samples: what each node did and who did it |
| `sample-run/` | one real run of it |

## What one real run did

```
  limits    answered  on 127.0.0.1:18101      crossed        39/   22 tokens
  storage   answered  on 127.0.0.1:18100      here           37/   23 tokens
  cache     answered  on 127.0.0.1:18102      crossed        38/   28 tokens
  editor    answered  on 127.0.0.1:18100      here          125/   69 tokens

  members that ran a node: 127.0.0.1:18100, 127.0.0.1:18101, 127.0.0.1:18102
  the coordinator was:     127.0.0.1:18100
  nodes crossed to:        127.0.0.1:18101, 127.0.0.1:18102
```

**All three members ran a node, the coordinator among them**, and that is the design rather than a
quirk of this run: a member that happens to be the one a caller asked is still a machine with a
gateway on it, so alpha is a candidate for its own work like any other. Round-robin took the three
in address order and came back round for the editor, which is why alpha has two of the four.
Picked for itself it *runs* the node — nothing is dispatched, and there is no round trip to its own
listener to pay for. A deployment with one live member is then just the ordinary case of that,
which is exactly `examples/graph-run`.

Round-robin is what makes the spread flat here; `random`, the default, would have given a
four-node graph over three members whatever three draws happen to give. Neither looks at how busy
a member is — that is the deferred strategy in `ROADMAP.md`, and a member publishes a heartbeat and
nothing else, so there is no load to be the least of.

Three things the run shows that a single process cannot.

**A placement says where a node *is* running, not where it ran.** The coordinator writes the
record when it sends the node and clears it when the node ends, so reading the plane after the run
correctly answers *nothing is running anywhere*. `run.sh` therefore samples it every 250 ms while
the stream is open, and `report.py` keeps the first address it saw per node. A node quicker than
the sampling interval can be missed, and the report says `not sampled` rather than guessing.

**A dispatched node's events are indistinguishable from a local one's.** The caller reads one
stream of one run; nothing in it names a member, and that is deliberate — where the work went is
the deployment's business, not the caller's. It is why the placement sampling exists at all: the
stream cannot answer the question.

**The answers outlive the run, keyed by run and node.** `kept-keys.txt` is the shared cache after
the run:

```
ip:sys:graph-run/c20f90068b32b69df116eeef735c032d/node/cache
ip:sys:graph-run/c20f90068b32b69df116eeef735c032d/node/editor
ip:sys:graph-run/c20f90068b32b69df116eeef735c032d/node/limits
ip:sys:graph-run/c20f90068b32b69df116eeef735c032d/node/storage
```

Four nodes, four answers, each under the run that paid for it — the two alpha ran itself included,
since what is kept is for whichever coordinator picks the run up and not for whoever ran the node. A coordinator picking this run up
would find them and not pay again. Only a node that spent something is kept, so a refusal leaves
no key — which is what stops a kept refusal answering every retry for a day.

## What it does not show

**A stop crossing to a node already running elsewhere.** It needs a node to fail while a sibling
is mid-turn on another member, which is a race to arrange and a certainty to assert, so it is
tested instead of demonstrated:
`a_stop_the_coordinator_published_ends_a_node_running_here` and
`a_node_dispatched_after_the_stop_was_published_is_cut_short_and_never_run`, both in
`src/routes/cluster.rs`.

**A member dying mid-node.** The coordinator fails that node and the graph with it rather than
running it again, because anything the member said means turns were taken and a rerun would
charge the tenant twice. Killing `beta` mid-run is a one-line way to watch it, and the caveat it
illustrates is in `ROADMAP.md`.

**Anything about a real network.** All three members are on loopback, so the secret is the only
thing authenticating them and nothing is encrypted. A cluster whose members are not on a network
you control needs TLS in front of them.
