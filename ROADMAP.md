# Roadmap

What is built and what is not. `DESIGN.md` is the specification; this file only says how far the
code has got against it.

An item is **shipped** when it is in the tree, exercised by tests, and reachable from a running
server. Anything else is listed under what is left, with what it waits on.

## Shipped

### Core types and configuration

- Newtypes for every identifier and key: tenant, user, api, nonce, signature, route key and target,
plugin checksum, order and scope.
- `TnKey`, `UtKey` and signature derivation, with the hex forms a request carries.
- One TOML file, rejected on a misspelled key, covering the listener, storage, cache, auth,
telemetry and upstreams. Secrets are wrapped so they do not print.

### Authentication and authorization

- Signed requests over the four `X-Ip-*` headers: tenant, user, nonce, signature. Both signing
schemes are checked, the ordinary user's and the tenant owner's.
- The signing headers are consumed at the edge and never reach an upstream.
- Roles (`SysAdmin`, `TenantAdmin`, `Member`) with explicit capability grants on top, scoped by
user, tenant and api.
- The system administrator is refused anywhere but loopback.
- Every refusal collapses to one status, so failures cannot be used to enumerate tenants or users.
- Nonce replay: each nonce is spent once per caller against the system cache, for `auth.nonce_ttl`.
Spending happens after the signature checks out, so a stranger cannot burn a caller's nonce.

### Proxy and dataflow

- The dataflow is a typestate machine: authorize, request headers, request body, forward, response
headers, response body. A stage cannot be skipped or reordered — the call that would do so does not
compile.
- Hop-by-hop headers are dropped in both directions; `Host` and content length are kept true.
- Server sent events stream through a chunk chain rather than being buffered.
- Routing: a rule is a key tuple to a non-empty list of targets, resolved by authority group then
longest path first. `/_ip` is reserved, and a rule that would shadow it is refused when the table is
built. Rules come from configured upstreams and from storage.
- Endpoints: `/_ip/healthz`, `/_ip/whoami`, and everything else proxied.

### Plugins

- WASM plugins as header, body and SSE chunk processors, run on wasmtime with fuel metering, epoch
deadlines and a memory cap per instance.
- Instances come from a pooling allocator with imports resolved once at load.
- Chains are built per request from stored rules, scoped globally or to a tenant, optionally
narrowed to a user and an api, ordered within each stage.

### Storage

- Two backends behind one trait: sqlite (`standalone-storage`) and postgres (`fast-storage`),
additive features, chosen by the `[storage]` backend tag. A tag naming a backend the build does not
carry stops startup with the feature name to rebuild with.
- Tenants, users, memberships, grants, routes and plugins, with migrations per backend.
- One conformance suite runs against both backends, so neither drifts from the other. Postgres tests
run against `DATABASE_URL` on a scratch schema of their own.

### Cache

- Two backends behind one trait: sled (`standalone-cache`) and redis (`cluster-cache`), chosen by
the `[cache]` backend tag, with the same unsupported-backend refusal as storage. One conformance
suite runs against both.
- Three levels — response, semantic, system — each keyed so no level can read another's entries.
- Request/response cache: keyed by sha256 over route key, tenant and request body, so a hit can only
be the same tenant asking the same route the same thing. A hit answers before the upstream call and
runs no response processor, spending no tokens.
- TTL, lowest to highest: the `[cache] response_ttl` default, then the response's own expiration
headers. `Cache-Control: max-age` or `Expires` names the span; `no-store`, `no-cache`, `private` or
a span already past keeps nothing.
- Only successful, non-streaming responses are kept. A cache that cannot be read or written costs a
round trip upstream and is logged, never a failed request.
- Per-level LRU limits, enforced by the sled backend: each entry weighs key plus value, each level
keeps its total and an index ordered by last use, and the oldest go until the level is inside its
limit.

### Management api and login

- Passphrase login (argon2) issuing the session cookie the web UI carries, `HttpOnly; Secure;
SameSite=Strict`; logout that keeps a token refused until it would have run out anyway; a session
endpoint saying who the caller is and what it may do where.
- One extractor behind every management endpoint, taking a session cookie or a signed request, with
a CSRF header required of cookie callers that change something.
- Endpoints for tenants, users, memberships, grants, routing rules, plugins and plugin chains, each
at the authority the capability model describes, and the first system administrator made by a local
CLI subcommand rather than by any api.
- Keys shown to a logged in session that gives its passphrase again, never to a signed call, with a
limit on wrong passphrases that ends the session it was reached in.
- Every list newest first, paged by `limit`, `offset` and `after`, with every time a unix second.
- Plugins owned per chain: wasm stored once under its checksum, a row per owner, compiled and
checked before it is stored, and removed when its last owner lets go.
- Rule changes propagated to every node: a revision moved on by database triggers, a published
snapshot written only when it is newer, a jittered reload, and one pointer swapped so a request is
never routed by one set of rules and processed by another. A cache left behind heals on a timer.
- An api suite in hurl, run against a server of its own by `tests/api/run.sh`.

### Measuring what a request costs

- A trace id per request, drawn here and never taken from a caller, on the answer as
`x-ip-trace`, with four spans under it: ingress request, egress request, ingress response,
egress response. Each carries the tenant, the user, the api and whether the cache answered.
- Requests grouped by the turn a caller marks with `x-ip-turn`, so one chat turn is one group.
- OpenTelemetry export to `telemetry.otlp_endpoint` over grpc, the exported trace named by the
same id the caller was answered with, and `telemetry.sample_ratio` deciding what share is
exported. No collector configured exports nothing and logs as before.
- A usage row per request: who spent it, on which api and model, the tokens in each direction,
whether the upstream or the cache answered, how long the caller waited, and the trace it can be
read back under. The tokens are read out of the answer itself, in the three shapes the upstream
families spell them in, a streamed answer included.
- An answer out of the cache is recorded with zero tokens and a cache marker, so the saving is
visible and a request rate still counts it.
- `GET /_ip/usage`, newest first and paged, narrowed by tenant, user and api. The system
administrator reads every tenant; anyone else names a tenant and must hold `Observer` in it,
which a tenant owner does by standing.
- `[usage] retention` sweeps away rows past their keeping on a timer, and keeps every row when
it is unset.
- The records of the last requests answered are waited for before the process ends.

## Shipped with a caveat

- **`no-store` disables the response cache against upstreams that send it.** Correct HTTP, and
several LLM APIs send it on completions, so the cache can be a no-op in production. Overriding it
would mean deliberately ignoring what the upstream asked for; the call has not been made.
- **A level can sit briefly over its limit.** Eviction runs as its own transaction after the write,
so a crash in between leaves the level over until the next write.
- **Redis enforces no per-level limit.** It evicts by the server's `maxmemory-policy`, so the limits
are configured on the sled backend alone and TTLs do the real work in a cluster.
- **Only one target per rule is used.** A rule may name several, but the first is always taken —
see the dispatch strategies below.
- **Only `http` and `https` are forwarded.** `ws`, `wss` and `tcp` parse and are refused at routing.
- **A rule change is not in force the instant the api answers.** The write is committed and
published, but each node waits a jittered moment before rebuilding, so nodes come into step within
seconds of each other rather than at once.
- **Only a grpc collector is exported to.** `telemetry.otlp_endpoint` is reached over
grpc, so an http endpoint such as `:4318/v1/traces` is not answered by; traces are dropped and
the failure is logged at startup.
- **Anthropic's prompt cache counts are not in the input tokens.** `cache_read_input_tokens`
and `cache_creation_input_tokens` sit beside `input_tokens` and are not added to it, so a
request served largely from the upstream's own prompt cache records the few tokens it was
billed for rather than the many it sent.
- **A request that never reached an upstream is not recorded.** A refusal before routing names
no api, and a request the upstream failed leaves no row, so request rates are counted over
answers alone.
- **A published rule set that cannot be built leaves a node on the rules it has.** It keeps serving
and retries, so nodes can serve different revisions while one of them cannot build the newest.

## To be implemented

Roughly in dependency order. Each entry names what it waits on.

### Management api, what is left of it

- Quota and rate limits per tenant and user — no representation for either exists yet.
- The signed half of the api is covered by the rust tests alone: a signature is sha256 over
hex-decoded keys, which the hurl suite cannot compute, so that suite exercises the cookie path.
- Syncing a list by `after`. Every list is newest first and takes `?after=<unix seconds>`, so a
client that records the newest `created_at` it has seen can ask for only what came since. Times
are whole seconds and `after` is strict, so a record made in the same second as the last one seen
is skipped; syncing on it needs a tie-breaker, such as the row id, carried beside the time.
- Wasm a deleted tenant owned alone is left in the store with no owner. Nothing lists it or runs
it, but nothing removes it either until a sweep of unowned plugins exists.

### Intelligent routing

- Dispatch strategies across a rule's targets: round robin, least load, ratio dispatch, hedging.
Waits on per-upstream load and latency being recorded.
- Difficulty classification into `easy`, `routine`, `median`, `hard`, `research`, with a local
decision tree first and an external arbiter LLM when confidence is low.
- Endpoint capability matching, so a request goes to the least capable endpoint that can serve it.
- Chat turn stickiness, so one classification serves a whole turn.
- Self learning: fold an arbiter's summary and reasoning back into the decision tree, plus the
best-effort endpoint that asks every upstream and aggregates or picks one.

### Semantic cache

- The third cache level: embedding the request body, an HNSW index over it, and a hit when meaning
matches rather than bytes. Waits on an embedding model and a live LLM endpoint, which is why the
level exists in the key space but holds nothing.

### System agents and run graph

- Agent templates stored in the database, with sandbox, tools and prompts.
- Ip-VFS: a virtual file system over object storage that agents see as ordinary paths, with a new
version per mutation and a CLI to sync up and down.
- The preset tool set (core utilities, python, js/ts, bash) and optional MCP tools, all sandboxed.
- Agent traffic routed through the gateway itself, so it is authenticated, authorized and processed
like any other request.
- Run graph: a `dr-strange` plane of agents in a DAG, run from `POST /_ip/graph_run/{id}`.

### Persistence beyond configuration

- Facts and chat session history as long term memory.
- Knowledge and RAG with a graph run.
- Spend and audit records. Usage is recorded; what a token costs, and an audit trail of who
changed what, are not.

### Web UI

- Login, the management menu (tenants, users, quota, authorities, LLMs, MCPs, tools, account) and
the gateway menu (rules, processors, agents, graph run, observe), plus a dashboard. Waits on the
management api.

### Observability, what is left of it

- Metrics: only traces are exported, and nothing counts requests, tokens or latency for a
dashboard to read.
- Logs to open-observe in cluster mode, to a local file standalone. Logs go to the process
output wherever it runs.

### Security

- Builtin processors that filter secrets and personal data out of requests and responses.
- Tenant and user isolation audited end to end rather than assumed.
- Optional eBPF rate limiting per source address, Linux only.
