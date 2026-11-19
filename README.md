# intelli-prism

An enterprise AI proxy and gateway, written in Rust. It sits between your callers and your LLM
upstreams, authenticating every request, deciding what each caller may reach, running plugins over
requests and responses, and answering a repeated question from cache rather than from the upstream.

## What it does today

- **Signed requests.** Four `X-Ip-*` headers carry tenant, user, nonce and signature. The signature
is checked against stored keys, the nonce is spendable once, and the signing headers never reach an
upstream.
- **Tenants, users and capabilities.** Roles with explicit grants on top, scoped by user, tenant and
api. Every refusal looks the same from outside, so failures reveal nothing about who exists.
- **Routing.** A rule maps an arriving endpoint to upstream targets, resolved longest path first.
`/_ip` is reserved for the gateway's own endpoints and no rule may shadow it.
- **A dataflow that cannot be reordered.** Authorize, request headers, request body, forward,
response headers, response body — each stage is a distinct type, so skipping one does not compile.
- **WASM plugins.** Header, body and server-sent-event chunk processors on wasmtime, with fuel
metering, epoch deadlines and a memory cap, scoped globally or per tenant, user and api.
- **A management api.** Tenants, users, memberships, grants, routing rules and plugin chains, over a
session cookie for the web or the signed headers for a script. Lists are newest first and paged; keys
are shown only to a session that gives its passphrase again.
- **Rule changes that reach every node.** A change moves a revision on inside the same transaction, is
published to the cache, and every node reloads and swaps its routing table and plugin chains without
stalling the requests in flight.
- **A record of what every request cost.** A trace id of our own on every answer, four spans
under it, and a row per request: the model, the tokens in each direction, whether the cache
answered and how long the caller waited. Exported to an OpenTelemetry collector when one is
configured, and read back over `GET /_ip/usage`.
- **What that cost in money.** What a model charges is a row set through `PUT /_ip/prices` and
carried to every node, so each usage row carries what it cost as well as what it spent, and the
summary totals a window beside the count of rows nothing priced. A model nobody has priced has
no cost, which is not a cost of nothing.
- **A record of who changed what.** Every management write records the account, the endpoint and
what the thing was before and after, in the same transaction as the change itself, so neither can
exist without the other. What a row carries is declared apart from how the thing is stored, which
is why no passphrase hash or tenant key is in it. Read back over `GET /_ip/audit`.
- **Agent templates, and runs of them.** What a system agent is — its prompt, its turn and time
limits, the tools it may call and the file tree it would see — stored for the deployment or for one
tenant and managed over `/_ip/agents`, with every part checked before it lands. `POST
/_ip/agents/{agent}/run` runs one and streams what it does as it does it: each turn, the text, every
tool call and its result, and a final event saying why it stopped and what it spent. A caller with
context the template does not hold — a database's schema, beside a SQL tool — adds it for one run at
`POST /_ip/agents/{agent}/run_with_prompt`, which appends it after the template's own prompt and
leaves the stored template as it was.
- **Tools over MCP.** The servers a template's tools live on are registered over `/_ip/mcps`,
addressed `server.tool`, and resolved against what each server really offers before a run's first
turn — so a tool that cannot be reached refuses the run rather than failing partway through it. A
token held for a server is written and never read back. A run's turns go through the gateway like
any other request, under the authority of whoever started it, so they are routed, filtered, cached,
metered, priced and limited the same way.
- **Dispatch across the endpoints behind a rule.** Round robin, least load or a share each, chosen
per rule and per configured upstream; a rule can also race a request across two or three of them
and take the first successful answer, when the body is one the gateway can send more than once. A
share of nothing drains an endpoint without touching the rule, and every usage row names the
endpoint that answered and how many it was raced across.
- **Quota, rate and spend limits.** What a tenant, one of its accounts or one of its apis may
spend, in tokens, in requests or in money, over a minute, an hour, a day or a month. The counts
live in the cache and are seeded from the usage rows; a caller past its limit is told with 429,
which of the three ran out, and when to come back. A money cap counts only what is priced, so it
is as complete as the prices are.
- **Run graphs, across machines.** `POST /_ip/graph_run/{id}` runs a DAG of agent templates and
streams it as it happens: a node's answer feeds its children, a node with several parents is given
all of their answers at once, and a generation really runs at once. A graph may declare a loop
back, the one cycle permitted: a node unsatisfied with what it was given answers
`{"loop_back": "survey", "notes": "the figures are for the wrong year"}`, and where the graph
declares that loop and its own cap has something left, that node and everything after it runs
again — the survey reading what the review made of its last answer, under `$again`. A node that
may double back is told so by the run itself, which names the steps it may send work back to and
how often, so no template has to repeat the graph in prose. An edge may also carry a condition, and
then which of the steps after a node run is that node's own choice: it is told each condition and
answers `{"branch": ["escalate"]}`, naming one of them, several or none. A step nobody named is
skipped, along with everything only it fed; a step after a branch runs on what arrived, with the
skipped parent's key absent; and a node that names nothing ends the run with its own answer, which
is a decision rather than a failure. A caller may give the graph's first nodes context of their own
at `POST /_ip/graph_run/{id}/with_prompt`, which each of them reads after its template's prompt and
before whatever the graph lets it ask for; a node with a parent gets none of it, since what it should
carry is the model running its parent's to decide, and that is not built yet.
Where a deployment is a cluster, its nodes are spread over the live members — the one the caller asked included, since it
is a machine with a gateway on it like the rest — and a node sent elsewhere streams back the same
events a local one says, the member authenticating itself with a shared cluster secret and
building the caller's authority from its own reads, so a dispatch buys the right to say which run
a node belongs to and never what that caller may do.
- **Looking a fact up, over a graph or a vector store.** A corpus is registered over
`/_ip/retrievers` — a `dr-strange` plane or a `qdrant` collection, belonging to one tenant — and
becomes tool addresses a template or a graph node may ask for: `search` for what is nearest,
`neighbors`, `rank`, `relate` and `cluster` for what a graph holds and a collection has not got. A
graph node may name retrievers of its own on top of its template's, so one template serves two
nodes that read different things. The query is embedded by this deployment's own `[semantic]`
artefact and the backend is handed numbers, never words and never a key, so nothing is spent
anywhere the usage rows cannot see it; a corpus whose vectors are another model's width is refused
while the operator is reading the answer rather than at search time. What comes back is one json
object a line, id first, and a look-up that goes wrong is a failed tool result the model is told
about rather than the end of the run.
- **Two storage backends.** sqlite for a single node, postgres for a cluster, behind one trait and
one conformance suite so neither drifts from the other.
- **Two cache backends.** sled for a single node, redis for a cluster, same arrangement. The
response cache answers a repeat of the same request from the same tenant without calling the
upstream; TTL comes from configuration unless the response's own expiration headers say otherwise,
and each cache level evicts its least recently used entries once it is full.

`DESIGN.md` is the specification. `ROADMAP.md` says what is built, what is built with a caveat, and
what is not built yet.

## Building

```
cargo build --release                                    # postgres and redis, the cluster default
cargo build --release --no-default-features \
  --features standalone-storage,standalone-cache         # sqlite and sled, a single node
```

Copy `intelli-prism.example.toml` to `intelli-prism.toml` and edit it. Every key is rejected if
misspelled, so a typo stops the server rather than silently taking a default.

## Testing

```
cargo nextest run --workspace                            # every crate, sqlite and sled
tests/api/run.sh                                         # the management api, over http
```

The postgres and redis tests check nothing unless `DATABASE_URL` and `REDIS_URL` name a server;
each works on a scratch schema or key prefix of its own. `tests/api/run.sh` starts a server on a
database and cache that go when the run does, makes a system administrator the way an operator
does, and runs the `.hurl` files beside it in order.

## Examples

```
examples/graph-run/run.sh                                # a five-node graph, a real model
examples/loop-back/run.sh                                # a graph that doubles back on itself
examples/branch/run.sh                                   # one node choosing between three ways out
examples/loop-and-branch/run.sh                          # a loop back and a branch in one graph
examples/cluster-run/run.sh                              # the same, across three machines
examples/retrieval/run.sh                                # looking a fact up, over both backends
```

`examples/graph-run` runs a graph that writes a merge sort in Rust and in Go, benchmarks each and
compares the two, against a live provider and a real dr-strange plane. It needs `IP_API_KEY` and
`drsg` 2.28.0 or newer; one recorded run is kept beside it, with what that run showed about
parallel generations, fan-in and unpriced spend.

`examples/loop-back` runs the smallest graph that shows a loop back: a reviewer that holds a
drafter to a house style the drafter was never told, so the first pass fails and the second
succeeds on the notes the reviewer wrote. The recorded run beside it keeps every step's input, one
of which is the feature in a single file, and the README says what the example was tuned to avoid —
including a model that sent work back while saying it had passed.

`examples/branch` sends three incident reports past one triage node with a condition on each of its
three outgoing edges, and each report leaves by a different one. Its README carries the finding that
cost the most to learn: a model asked *which of these* conditions holds is far more reliable than
one asked *whether any* does, so the third branch exists because the two-branch version named a
branch for a report that matched neither.

`examples/loop-and-branch` puts both mechanisms in one graph, which is the only way to reach the
state neither reaches alone: a node that answered, was passed over when the node that chose it ran
again, and had its answer dropped from the run's. The recorded runs keep every step's input, and the
README says what six rounds of tuning taught — including that a formatting example in a template
teaches the shape of the whole answer, and beats the advice the engine appends.

`examples/cluster-run` is the half of that a single process cannot show: three gateways sharing one
store, one cache and one plane, a graph started on one of them, and its nodes spread over all
three. It needs a postgres and a redis as well, since a cluster is machines that share a store and
a cache. The recorded run beside it shows which member ran each node — the coordinator included,
since it is a candidate for its own work — and the four answers left in the shared cache under the
run that paid for them.

`examples/retrieval` puts one corpus of twelve pages into both a qdrant collection and a
dr-strange plane, embedded by the deployment's own artefact, and asks the same question of each
through two graphs that differ only in the tools their `look` node names. It needs a qdrant — it
starts one in docker — and `IP_EMBED_ARTIFACT` beside the key. Its README keeps what three real
runs found, including the one that matters most: a node was handed a different node's answer by the
semantic cache, because that cache's question is the conversation's opening text and does not
include the tools declared with it, and the embedder a look-up needs cannot be configured without
that cache.

## Status

**This is not production software.** It is an unfinished project under active development, at
version 0.1.0, and nothing about it is stable — the configuration format, the storage schema, the
wire headers and the plugin interface all still change without ceremony.

Large parts of the design are unbuilt: there is no web ui, nothing that runs a system agent, and
no long term memory. A graph's nodes cross machines now, but a member is picked at random or in
turn and never by how busy it is, and a node that fails after its first event is not rerun
anywhere. The pieces that do exist are tested, but they have never run under real traffic, and no
security review has been done. Read `ROADMAP.md` before assuming a feature is there.

## License

Dual licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you state otherwise, any contribution you deliberately submit for inclusion
in this work shall be dual licensed as above, with no additional terms or conditions.
