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
- The one thing a plugin may import is `ip::log(level, ptr, len)`, which writes a line at the
level it names into the gateway's own logs and traces, under the span of the request that ran
it. A module importing anything else is refused at load. Lines are cut to
`PluginLimits::log_bytes`.

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
- `GET /_ip/usage/summary`, one window of the same narrowing answered as two numbers: how
many requests were answered, and how many went upstream. The difference is what hedging
spent. An answer out of the cache raises the first number and not the second, having sent
nothing.
- `[usage] retention` sweeps away rows past their keeping on a timer, and keeps every row when
it is unset.
- The records of the last requests answered are waited for before the process ends.

### Quota and rate limits

- A limit is a scope, a thing counted and a stretch of clock: a tenant, optionally narrowed to
one of its accounts and to one api, counting tokens or requests over a minute, an hour, a day or
a calendar month. Of the scopes a request falls inside, the narrowest decides, so a tenant's cap
can be tightened for one account or one expensive api.
- The running counts live in the cache, keyed by scope and by the stretch's own start, so a
stretch resets at its boundary with nothing to clear. A count the cache does not hold is seeded
by summing the usage rows since the stretch began, so losing the cache costs a query rather than
handing out a fresh allowance.
- A request is counted against every rate that holds it before the cache is asked, so an answer
the cache gives still counts as a request made. What an answer spent is counted against a quota
afterwards, from the same numbers the usage row is written from.
- A request that has run out is refused with 429, carrying `Retry-After` and a reason naming
what ran out. A rate that cannot be read lets the request through and says so in the log; a
quota that cannot be read refuses with 503 rather than spend unmeasured.
- `GET/PUT/DELETE /_ip/limits`, paged and newest first. A limit over a whole tenant is the
system administrator's to set; one naming an account is its tenant owner's, inside its own
tenant.
- Limits ride the same revision, publication and jittered reload as the routing rules, so a
change reaches every node the way a rule change does.

### Dispatching across the endpoints behind a rule

- A rule says how its requests are spread over the endpoints behind it: `round_robin` by default,
`least_load`, or `ratio` by the share each endpoint carries. Both a stored rule and a configured
`[[upstream]]` say it, so a replicated upstream can be described in the file alone.
- A rule may name a count as well, which races each request across that many of the rule's
endpoints at once. The count is capped at three and clamped to the endpoints a request may reach, so
a race is across what is live, at least one. The first successful answer wins; a failure on one arm
does not end the race, and the arms that lose are abandoned, paying back the load each had claimed
and reporting no latency of their own, so the least-load scores do not skew.
- A raced rule may also name `hedge_delay`, a wait in milliseconds between starting one arm and
starting the next. The default, zero, starts them together as before; a failed arm starts its
successor at once rather than waiting. A rule that races nothing keeps the wait it was given and
never spends it, so a rule raised to hedged later already carries its pacing.
- A rule whose first endpoint is healthy asks the upstream for one request rather than one per
arm, and the usage row counts the arms that actually went out, so the saving is visible in the
rows rather than only promised by the wait.
- A race is refused when the request cannot be sent more than once, and the request goes once
instead, to the one endpoint the strategy would have chosen: a body declared past what the gateway
will hold in memory, a body that declares no length, since an unknown size cannot be checked against
that limit, and a file transport.
- Round robin counts a rule's turn in the rule's own plan, worked out when the routing table is
built, so a dispatch costs one atomic add and no lock. Ratio draws against a running total found
at the same moment, so it costs a draw and a search rather than a sum per request.
- Least load scores each endpoint as what a request arriving now would wait behind: everything in
flight at what that endpoint has been taking. The counts are this node's own, in atomics beside the
rule, so the decision reads no lock and hashes nothing.
- What an endpoint takes is folded in away from the request that finished: a completed request says
how long it waited and moves on, and a folder works the average into the plan in batches.
- A share of nothing drains that endpoint: no strategy sends it a request until the share is
raised again. A target whose every endpoint is drained is dispatched to in turn all the same, since
a rule must have somewhere to send a request.
- Each usage row names the endpoint that answered it, by host and port, and the `egress.request`
span carries the same. An answer out of the cache names none, having reached none. And the row
says how many endpoints the request was raced across, one when it was not raced at all.

### Choosing an endpoint by the difficulty of the request

- Five difficulty classes order what the gateway carries: `easy`, `routine`, `median`, `hard`,
`research`. The order is the whole mechanism — the least capable endpoint that can serve a request
means nothing until requests and endpoints sit in one order.
- Every endpoint declares a ceiling rather than a set: the hardest class it serves, and with it
every class below. A set would let an endpoint serve `easy` and `hard` but not `median`, which is
no real machine. An endpoint that declares no ceiling serves any class but yields to every
endpoint that declared one, taking a class only when nothing which named a ceiling can carry it.
- Dispatch narrows, then the strategy decides: a class selects the lowest tier whose ceilings
carry it, and within that tier the rule's own strategy picks as it always did. Classification
chooses the tier, never the endpoint.
- When no endpoint can serve the class asked, the most capable one answers rather than the request
being refused: an ambitious label costs a dispatch, never a failure.
- The class comes from the request itself, named in `x-ip-difficulty`, or from a request header
processor that sets one, which is how a classifier of one's own plugs in. A request that names no
class goes across every endpoint behind the rule, exactly as it did before any of this: the
unclassified path is the old path, unchanged.
- A chat turn stays on the endpoint it landed on. The caller names the turn with `x-ip-turn` when
it sends one; otherwise the turn is recognised from the conversation, by a hash of its system
prompt and first user message, which every request of the conversation repeats unchanged. That is
the same prefix the upstream providers cache prompts on, so an affinity keyed on it keeps their
cache warm too, saving input tokens and not merely dispatch.
- A compaction ends a turn, caught whichever way it hides: a rewritten opening gives the
conversation a new anchor, and one that keeps its opening shrinks the count of messages, which
every turn keeps alongside where it landed.
- Where a turn landed is kept in the system cache under a short time to live; a memory that
cannot answer costs the affinity and never the request.
- Each usage row names the class it was dispatched at. An answer out of the cache carries it too,
since the class is the request's own and says nothing about who answered it.

### Naming the difficulty of a request

- Four levels answer in order, and each one fills only the silence before it: what the request
names in `x-ip-difficulty`, then what its turn remembers, then what the local model says when it
is confident enough, then the arbiter. A request none of them can call is dispatched
unclassified, across every endpoint behind the rule, exactly as it went before any of this.
- The local model is a multilingual encoder — a fine-tuned `mmBERT-base` exported to ONNX —
running in the gateway's own process. One forward pass over the opening of the conversation, no
generation and no network, and one artefact answers for English and Chinese rather than one per
language.
- What decides whether it commits is a margin, not a confidence: the distance between the two
likeliest classes. A model can be sure of `hard` with `median` close behind it, and that is
precisely the ambiguous request, which a raw confidence figure calls settled and a margin does
not.
- A turn is classified once. The verdict is written to the turn's memory with the endpoint it
landed on, so every later request of the conversation is dispatched from memory and costs
nothing — neither a forward pass nor an arbiter call.
- The arbiter is a larger model, asked only where the margin fell short, and through this
gateway's own routing: resolved, dispatched and recorded as any other call is, asked in the
family its own route leads to, and presenting the configured upstream's own credential rather
than the caller's. It answers inside a timeout the operator sets, and a slow, refused or
unreadable reply leaves the request unclassified.
- The margin threshold is configuration and the temperature travels with the artefact. Raising
the threshold classifies less traffic and gets more of it right; lowering it classifies more and
is wrong more often, which is a bill on one side and a misdispatch on the other.
- Each usage row says which of the four levels decided it, and the usage summary counts them over
a window. That is how an operator sees what their own traffic does — how much the local model
carried, how much the arbiter was paid for, how much nothing could decide — and moves the
threshold on their own evidence.

### Getting the classifier's artefact

- The weights are published separately from the gateway and fetched deliberately, once, by an
operator. The gateway itself never reaches the network to get them: a process that pulled a binary
at startup would undo the thing this product is sold on, and it is the first thing a security
review stops at.
- `intelli-prism model fetch` downloads and verifies; `intelli-prism model verify` checks an
artefact already on the machine and touches nothing. An air-gapped install — carry the artefact in,
verify it, serve with no route out — is a supported path and the one the fetch is a convenience
against, not the other way round.
- Verification is not a flag and cannot be turned off. Weights whose digest is not the one their
manifest names are deleted rather than left where a gateway would find them.
- Each quantization carries its own manifest: its own fitted temperature, its own measured margin
table, its own checksum. The gateway refuses to start on a manifest it cannot reconcile, because a
build running another build's calibration misroutes silently, and silent is worse than stopped.
- A container image carrying the model is published alongside the plain one, so an air-gapped
deployment moves one artefact instead of two. The plain image carries no classifier and does not
grow.
- What the model does, how well, in which languages and where it is weak is in `MODEL_CARD.md`;
how it was built and what would improve it is in `DATASET_CARD.md`.

### Speaking every family's api

- A route may say which family its callers send and which its endpoints speak, and a client
written against one is then served by another without knowing. The three carried are the
OpenAI, Anthropic and Gemini shapes of a conversation.
- Everything goes through one canonical conversation rather than between families pairwise: a
reader and a writer each, so a fourth family is one of each rather than one pair per family
already carried.
- A route naming one family on both sides, or naming neither, forwards the bytes it was handed
untouched — not re-serialised, not normalised. Translation is a thing an operator turns on for
a route and never a cost paid by every request.
- An answer crosses as it arrives, event by event, so a streaming client pays nothing in
latency for the crossing and sees its own family's events throughout.
- The model a caller names is carried across and never remapped. A mapping would be the gateway
guessing which model of another family an operator meant, and an upstream refusing a name it
does not know is the clearer answer.
- Reading all three families is also what lets everything that reads a request body work on any
of them. Before this, a conversation of `contents` and `parts` had no turn anchor, so it got no
stickiness, no class and no arbiter, and nothing said so.

### Taking out what must not leave

- An operator declares what a secret looks like and the gateway takes it out of every request
body and every answer, replacing it with a placeholder such as `[redacted:email:1]`. The answer
carries `x-ip-redacted` saying what was taken, counted by kind and never quoted.
- Four ways of recognising one: plain text, a prefix, a suffix, or an expression. A prefix or a
suffix reaches through the whole token, so `sk-` takes the key it announced rather than the three
characters that announced it. Ten shapes ship — provider keys, a PEM block, an email address, a
card number, a resident identity number, an account number — and an operator takes the ones that
suit them or writes their own beside them.
- Where the issuer published a check it is used: Luhn for a card, ISO 7064 mod 11-2 for a resident
id, mod-97 for an account. Those three are the default, because a kind that confirms itself can be
switched on without first measuring what it costs in false positives.
- The floor is configuration and a tenant may only add to it. There is no arrangement of
configuration in which a tenant is scanned for less than the floor, which is structural rather
than checked: a tenant's detectors are built from the floor's secrets and its own together.
- Placeholders are ordinal by first appearance, so one value reads as one placeholder wherever it
turns up and the model can still tell that two mentions are the same person. They carry no hash of
what they replaced: a truncated hash is stable, and would hand the upstream a few bits of the very
thing being hidden.
- A clean body is forwarded as the bytes that arrived, never re-serialised. The turn anchor is a
hash of the body, so a filter that rewrote every request would move every turn the day it was
switched on, and cost the upstream's own prompt cache with it.
- The filter runs after the plugin chain, so nothing a plugin adds can be smuggled past, and
before the response key is taken, so what the cache holds is what was redacted.
- A streamed answer is filtered as it streams, which needs text held back: a model writes a secret
token by token, and a filter that looks at one event at a time finds nothing at all. The window is
how much an operator is willing to pay for that in time to first token.
- What was taken is counted in the usage row, by kind, never by value. A row says nothing where no
filter looked, and nought where one looked and found nothing: the two are different facts.
- Tenant and user isolation is audited rather than assumed, in two halves. Every listing a store
carries is asked whether naming one tenant can return another's rows; every router the management
api nests is asked whether one tenant's member can reach another's. Both have a tripwire that
reads the source, so a listing or a router added without a case fails the suite.

### Answering what was asked before, in other words

- The third cache level. A question is embedded by a local model, matched against the questions
this tenant has already had answered on this route, and answered from the nearest of them when it
is near enough. A hit spends no tokens, runs no response processor and reaches no endpoint, as an
exact hit does.
- **Only a single unanswered turn is ever looked up or kept.** A body holding one user message and
no answer is a question; anything else is a conversation, where the context is most of the meaning
and matching on the question alone would answer it out of somebody else's exchange. Agent traffic
therefore never touches this level. The test is per family and not a count of messages: one family
puts the standing instruction among them and the other beside them, so the same single question
counts one message written one way and two written the other.
- **How near is the operator's, with no default.** This is the number that decides when the gateway
hands one caller another caller's answer, and the only honest way to pick it is against real
traffic — so every answer carries `x-ip-cache: semantic` and `x-ip-nearness`, and the threshold is
moved against what your own questions actually score. An exact hit stays silent, because the same
bytes to the same question is not something a caller needs told.
- The usage row says which cache answered. An exact hit and a semantic one are different promises,
and after an incident the only question anyone asks of this level is which answers were
approximate.
- Isolation is structural in two places rather than filtered in one. Each tenant and route has an
index of its own, so a scan cannot reach anyone else's vector; and the stored answer is named by
the tenant, the route, the model and the vector together, so two indexes that somehow did exchange
keys would read nothing under them. Audited from both sides, with a tripwire that reads the key's
own parts out of the source.
- The model's identity in every key is the digest of its weights, not a version string somebody
remembered to bump. Two models' vectors are not near or far from each other but incomparable, so
replacing the model cannot serve an answer found by the old one's geometry.
- A vector is normalised the moment it is built, an answer that says `no-store` is kept in neither
cache level, and an embedder that is slow, busy or broken costs a miss and never a request.
- The model is the operator's, declared by a manifest beside the weights: the width, the length it
was trained at, how its token vectors become one, and any prefix it wants of a query. Verified
against `multilingual-e5-small` quantized to int8 — 384 numbers in about 6 ms, which scores a
paraphrase at 0.978 against 0.762 for two questions with nothing in common. That floor is why the
threshold belongs near the top of the range and why the figure is nobody's to default.

### Counting what the gateway does

- Nine instruments pushed to the same collector the traces go to, so an operator who has
configured one endpoint has configured observability. Nothing new listens and nothing new has
to be guarded.
- What a request was: answered by tenant, api, model and where the answer came from; tokens in
each direction; and how long it took, in buckets that run from a cache hit to a minute because
the defaults stop at ten seconds and an llm call does not.
- What was decided: the class each request went at and the level that named it, counting the
ones nothing would commit to — the share no level will call is what says whether the classifier
is earning its forward pass.
- What it cost: requests a limit turned away and which quota ran out, arms actually sent
upstream against answers given, and secrets taken out.
- **What the semantic cache was asked, and what came of it**, which is what M17 told operators
to tune against and could not show them. Every turn is counted — hit, miss, never eligible, or
the encoder would not answer — because a question that was never asked and one that nearly hit
want opposite responses and look identical in a request count. And the nearness of the nearest
candidate goes in a histogram on *every* lookup, not only the ones that hit: a bulge just under
the threshold is the picture that says lowering it would pay, and a hit-only histogram cannot
draw it.
- Metrics are not sampled, however traces are. A rate counted from a tenth of the requests is a
tenth of the rate.
- Every counter carries the tenant; neither histogram does. A counter with a tenant costs one
series per tenant, a histogram one per bucket per tenant — and what a particular tenant waited
is in their usage rows, per request, to the millisecond.

### Saying where the logs, and the numbers, are read

- Logs go to the collector the traces and the metrics go to, over the same endpoint and the
same transport. One `otlp_endpoint` configures all three signals, which is what its name has
promised since it was written.
- The export never silences the process output. Whatever `docker logs` or `journalctl` showed
before shows the same afterwards, because a gateway whose only log sink is the one that has
just failed is a gateway nobody can diagnose.
- The bridge does not carry the export's own lines. A collector that starts refusing batches
makes the exporter say so, and shipping that line would hand it back to the exporter it came
from; a failure that feeds itself is the one failure a log pipeline must not have.
- A rolling file beside it, for the deployment that runs no collector: a new file each day, as
many kept as were asked for, written off the thread that logged. The path is checked when the
configuration is read rather than at the first line, because the appender would make a missing
directory rather than refuse, and a mistyped path would quietly become a tree nobody reads.
- **A scrape endpoint, for the deployment that would rather not run a collector at all.** Its
own listener on its own address, off unless configured, serving one path and nothing else. Pull
and push are independent: either, both, or neither. A bearer token is offered rather than
required, because the deployment this exists for binds it to the loopback and is scraped by the
agent beside it.
- Until this, no collector meant no instruments: the meter provider was built only when an
endpoint was configured, so a standalone gateway counted nothing it could show. The provider is
now built from whichever readers there are.
- Redactions are counted by kind. The filter already kept the tally kind by kind; it simply
never travelled as far as the meter. The sum over kinds is what the counter said before it knew
the difference, so a dashboard built on the total keeps reading.

### Putting a price on what was spent

- **A price is a row, not a line in a file.** `(model, input, output)` in micros of the
deployment's currency per million tokens, set through `PUT /_ip/prices` by the system
administrator and carried to every node by the publication channel and the revision the routing
rules already use. What a model costs changes without a restart and without editing a file on
every host — which is what rules out the configuration file that would otherwise be the obvious
home for it.
- Money is integers all the way: micros of one currency in the stored price, in the row, in the
api and in the counter, with no float anywhere to accumulate error. A rate is refused above a
million currency units per million tokens, so a decimal point in the wrong place is caught at
the door rather than stored and believed.
- **A model nothing prices has no cost, which is not a cost of nothing.** The column is
nullable, the api omits the key rather than sending zero, and the counter mints no series at
all — while a model priced at zero records zero in all three, because *free* and *unpriced* are
different facts and a deployment running local models needs to tell them apart.
- Every answer is priced as it is recorded: the winner's tokens in both directions, plus the
prompt once more for each arm that lost a race, which the upstream read and billed for whether
or not it won. An answer out of the cache costs nothing against a price it has, which is the
number to read against what the cache saved.
- `GET /_ip/usage` reports `cost_micros` per row, and `GET /_ip/usage/summary` totals the window
beside `unpriced` — how many of its rows it could not price. A total missing rows reads as
incomplete rather than as a smaller bill.
- `ip.cost` beside `ip.tokens`, in micros, under the same labels.

### Capping what is spent

- **A money cap is a limit like the other two**, not a budget system beside them. `counted:
"money"` on `PUT /_ip/limits`, over the scope, the period and the api a limit already has, with
the allowance in micros — five currency units a month is `5000000`. No migration and no new
endpoint: the column stores the name of what is counted, and `money` is a new one.
- What is spent comes from the same rows the dashboard reads, summed over `cost_micros`, seeded
into the cache counter the way a token quota is seeded. A node that loses its cache re-reads the
rows rather than handing a tenant a fresh allowance.
- The refusal is the 429 every other limit gives, with the moment the next stretch begins, and
it says `money` where another says `tokens` — in the message, in the log and on `ip.refused`. A
caller that already backs off on 429 needs no change, which is why it is not a 402.
- **A money cap is a quota, not a rate**: a count it cannot read refuses. A rate fails open
because it protects an upstream; a quota and a cap fail closed because they protect a bill.
- An answer out of the cache costs nothing and moves no cap, which is the same arithmetic that
makes a hit free in the ledger.

### An audit trail of who changed what

- **A change and the record of it are one transaction.** The row is written over the same
connection as the change it describes, so neither can exist without the other: no change lands
unrecorded, and a record whose change rolled back is not a state the store can be left in. There
is no public method for writing one, because a public method for writing one would be a way to
record a change that never happened.
- **What a row carries is declared apart from how the thing is stored**, and that separation is
the whole point. A passphrase hash, a tenant's root key and an upstream credential are absent
because nothing put them there on purpose, so a column added to a table is missing from the trail
until somebody decides it belongs. The table is meant to be kept for years and read by people who
need none of those.
- **A row names what the api names**: the account, the endpoint as the router spells it, the kind
of thing, and which one in the spelling its own endpoint uses — `api_access on alice@acme/chat`,
`alice@acme`, the checksum of a plugin — plus what it was before and what it became.
- **A refused change records nothing, and neither does a change that changed nothing.** Granting
a capability already held writes no row: one saying it was handed over on a day nothing happened
would put its arrival on the wrong date, which is worse than silence.
- **Taking a member out of a tenant records each grant it lost, one row apiece.** A count cannot
answer which capability somebody left with, which is the question an incident actually asks.
- Read over `GET /_ip/audit`, newest first and paged, narrowed by tenant, actor and kind. The
system administrator reads every tenant; anyone else names the tenant it is asking about and must
hold `Observer` inside it, which an owner does by standing.

### What a system agent would be

- **A template is one thing, checked whole.** The id, the name, the sandbox and the config are
types that refuse what they cannot hold — a prompt past 32 KiB, more than 64 tools, the same tool
twice, a request timeout outliving the run it is part of, a name padded with whitespace — so a
template that reached the database is one every part of which was checked. Nothing is stored and
found wrong later.
- **The sandbox is a tree of names, not a list of paths.** A directory holds entries and a file
names an object location, so `bin/python` is a file called `python` inside `bin` rather than a
string with a slash in it: a segment may not contain `/` or `\`, and `.` and `..` are refused
rather than normalised away. At most 4096 entries and 32 deep, counted by an iterative walk,
because a tree deep enough to refuse is deep enough to exhaust the stack on the way to refusing
it.
- **A template belongs either to the deployment or to one tenant**, the way a plugin does: the
owner is nullable on the row and one unique index spans it, so a single id names a different
template under each owner without the two ever colliding. A read for a tenant nobody created
reports it missing rather than answering with the deployment's own.
- **Managed over `/_ip/agents`** — written with `PUT /_ip/agents/{agent}`, read one at a time or
paged newest first, removed with `DELETE`. The system administrator manages the deployment's own
and may name any tenant; anyone else names the tenant it is asking about and must hold `SysAgent`
inside it, which an owner does by standing. **This is `SysAgent`'s first endpoint**: the
capability has existed since the second milestone with nothing authorizing on it.
- **Every write is in the audit trail, all of it.** A template holds no secret to leave out — a
prompt, a tool name and an object location are what the operator wrote — so the row carries the
whole of what it said before and after, unlike a key or a passphrase hash that is deliberately
absent.
- **The types live in their own crate.** `ip-agents` depends on no other crate in this workspace,
including `ip-core`: an agent template is not a gateway concern, and the id rule is written again
there rather than shared, so that `ip-core`'s error type is not part of how this crate fails.

### Running what an agent template describes

- **An MCP server is registered, not configured.** `/_ip/mcps` holds the servers a template's tools
may live on — an id, a name, an endpoint and what it requires — written by the system administrator
alone, because a server is reached by the deployment itself and not on any one tenant's behalf. A
token written is never read back, the way a tenant key and a passphrase are: what the api reports is
whether a credential is required, never which one.
- **A tool is addressed `server.tool`**, resolved against what the servers really offer rather than
trusted. A template naming a bare tool, a server this deployment has not registered, a server that
will not answer or a tool no server offers refuses the run — before its first turn, so a typo costs
no tokens. What the model is told is `server__tool`, because every provider's function-name rule is
`[a-zA-Z0-9_-]{1,64}` and a dot is not in it.
- **A run's turns go through the gateway**, as requests the deployment makes on behalf of the account
that started it: routing, dispatch, hedging, the filter, the semantic cache, metering, pricing,
limits and the usage row all apply, because it is the same path a proxied call takes. A run is not a
second way out of this process.
- **Which model answers is settled in three steps**: what the request named, then what the template
named, then the model the upstream was configured with — which is what `[[upstream]] model` has
always meant. A template may therefore name no model and still run wherever it is deployed, and a
request may name one to override it for that run alone. Nothing left to name one is the request's own
omission and is answered 400, which a configured deployment cannot reach, since every upstream
carries a model. Each turn's `turn` event reports the model that *answered*, which is not always the
one asked for: a provider may route a request to a model of its own choosing and say so in its reply.
- **The credential is attached by the flow, not by the runner.** A deployment run marks its request
as presenting the deployment's own key, and the table hands dispatch the credential of the upstream
the rule names. The runner never holds one, so a run cannot present one provider's key to another —
which was possible for as long as the runner was the thing that chose.
- **A run goes out as whoever started it** — their real standing inside the named tenant and their
real grants. Nothing is synthesised and nothing is widened: an account that could not proxy to an
api by hand cannot reach it through a template either, and the turns spend that tenant's quota.
- **The loop ends on a limit, never on a tool.** A tool the model invented, a server that has gone
away and a tool that failed are all answers the model is given and may act on, because a model told
its tool failed often tries another. What ends a run is the turn ceiling, the time limit, a turn
past its own timeout, or the gateway refusing — and it says which.
- **It is streamed as it happens.** `POST /_ip/agents/{agent}/run` answers an event stream: `turn`,
`text`, `tool_call`, `tool_result`, and a final `done` carrying the answer, why it stopped and what
it spent. A call and its result are two events because the tool runs between them: a watcher told
nothing until the result would learn what was asked for only once it no longer mattered. Everything
that can refuse a run is settled before the stream opens, so a refusal is a status code rather than
one event inside a 200.

## Shipped with a caveat

- **Nothing in the sandbox runs.** A template's file tree is stored, checked and read back, and no
part of it is mounted, fetched or executed: a run reaches its tools over MCP and nothing else. The
sandbox remains a description waiting for Ip-VFS.
- **`use_best_effort` and `best_effort_strategy` are carried and read by nothing.** They are
stored, checked against each other — a strategy named without the flag is refused — and answered
back unchanged, as at M23: the loop does not consult them. They are in the api now so that a
template written today needs no rewriting when something does.
- **An MCP token is unencrypted at rest.** It is stored as written and never reported back, but the
column holds it in the clear, so the database is as sensitive as the tokens in it. Encrypting it
needs a key the deployment manages, which is not built.
- **A run is not resumable and nothing lists one.** It lives as long as its stream: a client that
hangs up loses it, nothing records that it happened beyond the usage rows its turns wrote, and
there is no handle to ask after one. The run id is generated, streamed, and stored nowhere.
- **The run graph composes nothing.** One template, one loop, one model. An agent calling another
agent, a template naming a sub-agent, a plan split across several — none of it exists.
- **Nothing rate-limits a run as a run.** Each turn is counted and capped like any other request,
so a run is limited turn by turn; the run itself is not. A template with a high turn ceiling is a
loop that may make many requests before a per-request limit notices.
- **A run reports no cost.** `done` carries a `cost_micros` and it is always zero: the gateway works
out what a request cost when it writes the usage row and reports it on no response header, so the
loop cannot see it. A run collects a trace id per turn, which is what the cost is recoverable by
afterwards — through `/_ip/usage`, not through the stream.
- **The administrator can start no run.** It manages every tenant's templates and holds a standing
inside none, so it has nobody to charge and no role to run as, and is refused. Running a tenant's
template for it means being a member of that tenant.
- **A run that pinned no family is refused where several are served.** The gateway routes by path
before anything chooses an endpoint, so `auto` picks among the endpoints behind one upstream and
cannot pick across providers. A deployment serving one family runs an unpinned template; one serving
several refuses it rather than sending it to whichever was configured first.
- **A run does not keep one endpoint across its turns.** M11's turn affinity is driven by a field the
runner does not set, so each turn is dispatched on its own and a multi-turn run may be spread across
the endpoints behind its rule.
- **A tool's arguments are passed as the model wrote them.** A call whose arguments are not an object
is run with none rather than refused, and nothing checks them against the tool's schema before the
server is asked: the server's own refusal is what catches it, and comes back as a failed result.
- **A sandbox path is not checked against object storage.** An object location is checked for
shape and for nothing else: no bucket is contacted, no scheme is validated, and a template may
name an object that does not exist or a store this deployment cannot reach. That check belongs to
Ip-VFS, which is unbuilt.
- **No node is told that a template changed.** Every other management write publishes a revision
and each node rebuilds what it holds; a template write publishes nothing, because nothing holds a
template in memory to rebuild. A runner will need that path and it is not there.
- **`no-store` disables the response cache against upstreams that send it.** Correct HTTP, and
several LLM APIs send it on completions, so the cache can be a no-op in production. Overriding it
would mean deliberately ignoring what the upstream asked for; the call has not been made.
- **A level can sit briefly over its limit.** Eviction runs as its own transaction after the write,
so a crash in between leaves the level over until the next write.
- **Redis enforces no per-level limit.** It evicts by the server's `maxmemory-policy`, so the limits
are configured on the sled backend alone and TTLs do the real work in a cluster.
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
- **A quota can be overshot by one request.** The tokens an answer spends are known only once it
has been given, so a request that takes a quota past what it allows is answered, and the next one
is refused.
- **A spent quota refuses even a request the cache could have answered for nothing.** Every
limit is checked before the cache is asked, which is what makes a hit count as a request; the
same order means a hit is not served once a quota is spent.
- **`LimitMgr` is not consulted.** The capability is scoped to an api, so a grant of it can never
match a limit that names no api. The limits endpoint goes by ownership instead, which is what
`DESIGN.md` describes; letting a granted member manage limits needs the capability's scope shape
widened.
- **Whether a request may be raced is decided the HTTP/1.1 way.** A request carrying neither a
content length nor a transfer encoding is read as having no body at all, which for HTTP/1.1 is what
the specification says. The inbound server speaks only that, so the reading holds. Serving HTTP/2
would break it: an h2 request may carry a body with no declared length, and such a request would
then be raced and its body held once per arm, with nothing checking its size. Turning inbound h2 on
means teaching that decision about it first.
- **A load score is one node's own.** Each node counts what it has in flight and what its own
requests have been taking, so a cluster spreads its traffic by each node spreading its own. Nodes
carrying very different traffic will not balance each other.
- **A drained target still answers.** Draining every endpoint behind one rule leaves it dispatching
in turn rather than refusing, since a matched request must go somewhere. Draining a whole rule is
done by removing the rule.
- **A rule change resets that rule's turn.** The turn lives in the plan built with the table, so a
rewritten rule starts again at its first endpoint. Only that rule's, and only the order, not the
spread.
- **A published rule set that cannot be built leaves a node on the rules it has.** It keeps serving
and retries, so nodes can serve different revisions while one of them cannot build the newest.
- **The classifier's artefact is not in this repository and must be supplied.** A directory
holding the exported model, its tokenizer and its gate, produced by the training spike and
pointed at by `[difficulty].artifact`. A gateway without one runs with no classifier at all,
which is a supported configuration and the path every request took before M13.
- **The classifier needs a build feature and a native runtime.** Only a binary built
`--features classifier` classifies, and a `[difficulty]` stanza in a build without it is refused
at startup rather than ignored. The ONNX Runtime library is loaded dynamically, so nothing is
needed to compile and it must be present to run — the part that surprises whoever packages this.
- **The model is as accurate as it is, and the margin chooses where that lands.** No figure is
promised here: a higher threshold classifies less and gets more of it right, the usage summary
says what an operator's own traffic did, and the threshold is theirs to move. `research` is the
weakest class, and the one a mistake costs most in — it is dispatched to a model that cannot do
the work.
- **An arbiter reply the gateway cannot read leaves the request unclassified.** Prose, a refusal,
a reply naming two classes or none: each is logged and dispatched without a class rather than
guessed at. A rising count of them is a prompt that needs fixing, and it is visible in the
`undecided` count of the usage summary.
- **A translation is best-effort, and what it cannot carry is dropped.** The request is served
without it, the answer names what went in `x-ip-untranslated`, and a warning is logged. The
alternative was refusing a request a client cannot change; the cost of this choice is a billed
answer that did not see everything it was sent.
- **A streamed crossing cannot name what it dropped.** Headers go out before the body does, so
there is no header left to set by the time a stream is being translated. What the request lost
is logged, and a streaming caller cannot see it.
- **A crossing that is not streamed holds the request and the answer whole.** Only on routes
that bridge two families, and only until each is rewritten — but it is memory a passthrough
route never uses.
- **Multimodal content does not cross.** Images and other non-text parts are dropped and named
like anything else this pass cannot carry; the text of the message goes on without them.
- **Two calls to one function in a turn pair by order when Gemini is involved.** That family
identifies a tool call by the function's name where the others carry an id, so the gateway
synthesises one from the name and the position. Answers arriving in a different order from the
calls would pair wrongly — which Gemini itself cannot express either, so it is a limit of the
protocol rather than of the crossing.
- **A cache hit records no class and no level for a remembered turn.** The response cache answers
before the turn's memory is consulted, so such a row says only what the request itself named. The
classifier count is therefore a count of openings judged, not of conversations.
- **A request is held in memory, up to 1 MiB, whenever turn affinity is configured.** Deriving a
turn's anchor needs the opening of the body, so a request that would once have streamed through is
now buffered to that limit; before M11 only a hedged or cached request held anything of its body.
There is no configuration key to turn it off. The trade was taken deliberately for M11 and is
recorded here, not re-argued.
- **A client that sends its own `x-ip-turn` gets no compaction detection.** A marked request never
has its body read, so no message count is kept for it and neither test for a compaction can fire.
Such a client must change its turn id when it compacts, or its turn never ends.
- **A body the gateway may not hold goes unfiltered, and the answer says so.** Filtering means
reading, and reading means buffering what was promised to stream. Such a request carries
`x-ip-redacted: unscanned: not held` rather than passing in silence, and so does a body that is
not json. An operator can see the hole rather than assume there is none.
- **A streamed answer costs time to first token to filter.** Text is held back until nothing is
still growing into a match — 256 characters by default, and `window = 0` holds nothing, which
catches only a secret arriving whole in one event and is honestly named `unscanned: streamed`. A
match longer than the window outruns it and goes out in pieces; a PEM block is longer than the
default.
- **Filtering a stream normalises its events.** They are read into the canonical conversation and
written back, so a family's own fields that the canonical form does not carry are not carried
here either. It applies only when filtering is switched on, and an event the reader makes nothing
of is forwarded exactly as it arrived rather than dropped.
- **A streamed answer's tally cannot ride its headers**, which went out before its first event.
The header says only that the stream was filtered; the count reaches the usage row and the log.
- **Redaction is one-way.** Nothing is kept that could put a secret back, so a caller whose own
key was redacted sees the placeholder in the answer. There is no vault to leak and no state to get
wrong under streaming, which is why it was built this way.
- **A tenant's own secrets are configuration, not stored rules.** An operator edits them; a tenant
cannot manage its own through the management api, and the scoping is per tenant rather than the
user and api narrowing a plugin rule allows. Precomputing a set per tenant is bounded and per
user and api is not, which is the reason.
- **A hedge's wait is a fixed duration per rule, tuned by hand.** It is read against what an
endpoint actually takes, and a delay set too short spends the arms the staggering was meant to
save. The adaptive version would need a latency distribution the gateway does not keep, which is
why it was not built.
- **The semantic index is in memory, in one process.** The answers are shared through the cache
like everything else; the vectors that find them are not, because a shared cache has no way to be
asked *what is near this*. So each node's hit rate is built from the traffic that node has seen,
and a restart starts over — the answers outlive it, unreachable, until their ttl. The cluster
already has a publication channel for propagating rules, and that is the route to sharing vectors
when it is worth building.
- **A lookup is an exact scan, not the HNSW graph `DESIGN.md` names.** Measured before it was
built: at the size a per-tenant, single-turn index actually reaches, a scan is *faster* than an
approximate search as well as being exact, and the crate that builds the graph cannot grow one, so
every eviction would mean a rebuild longer than the entries it rebuilds. The cost is that a lookup
is linear in what a tenant holds, which `capacity` bounds. The day a tenant's index is genuinely
large, what changes is the body of one method.
- **Every request that misses pays one forward pass of the encoder** before it goes upstream. The
level only earns its keep where enough questions hit, which is what the reported nearness is for.
- **The semantic level is per tenant, not per user.** Two people inside one tenant share answers,
exactly as they already do through the response cache, whose key has never been per user either.
With an approximate match that is a looser promise than with an exact one, and it is recorded as a
decision rather than left to be inferred.
- **No image ships an embedding model.** The classifier has a `--target classifier` image with its
artefact inside; the embedder has no equivalent yet, so a deployment that wants this level builds
with `--features semantic` and mounts an artefact of its own.
- **The tenant is a label on every counter, and that is the operator's cost to manage.** Taken as
a ruling, knowing the arithmetic: a tenant count that runs to thousands multiplies each counter
by it, and the tenant count is not ours to bound. It is there because *which tenant is doing
this* is the question a multi-tenant gateway exists to answer. Where it becomes too much, the
usage rows hold the same figures per request and are the better place to ask.
- **The scrape endpoint is as private as where it is bound.** What it serves carries a tenant
label, which is tenant data, and the token is optional. An operator who binds it to a routable
address and sets no token has published their tenants' traffic shape. It is off by default and
the configuration says this; the gateway does not refuse to serve it.
- **A clean request no longer adds a zero to the redaction counter.** Counting by kind means
there is no name to count a nought under, so a filtered request that found nothing adds no
series rather than a series holding zero. Absent and zero read the same through `rate` and
`sum`, and the usage row still records the clean nought, which is where that distinction
belongs — but a dashboard graphing the raw series will see one fewer.
- **Log lines are not filtered.** M16 takes secrets out of bodies, not out of what the gateway
writes about them. A log line is written by our own code and holds what our own code put there,
which is why this has not bitten; it is still a place a secret could reach the collector.
- **A hedged request's cost is arithmetic, not a bill.** Only the winner's answer is measured,
so each arm that lost is costed at the prompt the winner was billed for. An upstream that
charges an abandoned arm differently, or that hit its own prompt cache on one arm and not
another, makes the figure wrong in either direction. What is exact is the unhedged request,
which is almost all of them.
- **One currency, and it has no name.** Every price is micros of whatever the deployment counts
in, and nothing converts or labels it. A deployment buying from vendors who bill in different
currencies converts before setting the price, and nothing in the gateway will catch it if it
does not.
- **A price is the deployment's, not a tenant's.** It is what the vendor charges, with no
per-tenant markup: what a tenant is *charged* is a calculation over these rows and belongs
wherever the invoices are made.
- **A row keeps the price in force when it was answered, and is never re-priced.** Changing a
price changes what the next request costs, not what last month cost — which is what a ledger
should do, and does mean rows answered before anyone set a price stay unpriced forever. Pricing
a model is worth doing before the traffic, not after it.
- **A money cap is only as complete as the prices.** A row nothing priced spends nothing
against it, so a model nobody has priced is a model with no cap on it. This is the deliberate
side of the trade: the alternative, refusing while anything in the window is unpriced, turns one
forgotten price into an outage for every capped tenant, including their traffic to models that
*are* priced. `GET /_ip/usage/summary` reports `unpriced` beside the total, which is where an
operator sees the hole before it matters.
- **A money cap overshoots by the request that crossed it**, exactly as a token quota does and
for the same reason: a cost is known only once the answer is. A single request can therefore
carry a tenant past its cap, and the next one is refused.
- **A cap refuses; it does not warn.** There is no soft threshold, no alert and no webhook. An
operator who wants to know before the wall reads the summary.
- **Nothing prunes the audit trail.** Usage rows have a sweep and a keeping period; audit rows
have neither, so the table only grows. That is the right default for a trail — a retention policy
that quietly deletes evidence is worse than a large table — but it is the operator's to manage,
and a deployment that needs rows dropped on a schedule has nothing here to do it with.
- **A row is an ordinary row.** Nothing makes the trail tamper-evident: there is no hash chain,
no signature and no append-only enforcement below the api, so anyone with write access to the
database can edit a record or delete one. The trail answers what the gateway did, to a reader who
trusts the database; it is not evidence against whoever holds it. Hash-chaining it is a different
feature with a different threat model, and claiming that without building it would be worse than
not having it.
- **A trail records management writes, not traffic.** What a request did is the usage ledger's
business; this table answers who changed the configuration. Neither is derivable from the other.
- **What belongs to no tenant is the administrator's alone to read.** An account, a route and a
price are recorded against no tenant, and a tenant-scoped caller must name a tenant — so a tenant
owner cannot read who created an account inside its tenant, or who changed a route its traffic
takes. This is the isolation working as intended rather than a gap, but it does mean a tenant's
own trail is narrower than a tenant owner may expect.

## To be implemented

Roughly in dependency order. Each entry names what it waits on.

### Management api, what is left of it

- The signed half of the api is covered by the rust tests alone: a signature is sha256 over
hex-decoded keys, which the hurl suite cannot compute, so that suite exercises the cookie path.
- Syncing a list by `after`. Every list is newest first and takes `?after=<unix seconds>`, so a
client that records the newest `created_at` it has seen can ask for only what came since. Times
are whole seconds and `after` is strict, so a record made in the same second as the last one seen
is skipped; syncing on it needs a tie-breaker, such as the row id, carried beside the time.
- Wasm a deleted tenant owned alone is left in the store with no owner. Nothing lists it or runs
it, but nothing removes it either until a sweep of unowned plugins exists.

### Intelligent routing, what is left of it

- Self learning: fold the arbiter's verdicts back into the local model that could not call them.
With a trained model rather than a decision tree that is not a code path but a pipeline —
collecting what the arbiter said, assembling training data, retraining and exporting a new
artefact — and it is the weight of that pipeline, not a gap in the gateway, that keeps it unbuilt.
- The best-effort endpoint that asks every upstream and aggregates or picks one. Unrelated to the
classifier, and unchanged.

### Semantic cache, what is left of it

- An index the nodes of a cluster share, over the publication channel the rules already use.
Today each node learns only from what it has seen.
- A per-route or per-tenant threshold. One number governs every llm route, which is the right
default and the wrong answer for a deployment whose routes differ in what a near miss costs.
- An image with an embedding model in it, as `--target classifier` carries the difficulty one.

### System agents and run graph

- Something that runs a template: the sandbox mounted, the tool set resolved, the turns taken and
the time limit enforced. The templates themselves are shipped, above.
- Ip-VFS: a virtual file system over object storage that agents see as ordinary paths, with a new
version per mutation and a CLI to sync up and down.
- The preset tool set (core utilities, python, js/ts, bash) and optional MCP tools, all sandboxed.
- Agent traffic routed through the gateway itself, so it is authenticated, authorized and processed
like any other request.
- Run graph: a `dr-strange` plane of agents in a DAG, run from `POST /_ip/graph_run/{id}`. A node
with several parents receives their answers as a json object keyed by parent, a node that does not
answer stops the graph, and the graph's only ceiling of its own is a node count — money and time
stay with the per-turn and per-tenant limits rather than being counted a second time. The order to
walk the nodes in, and the check that the graph is acyclic, are asked of the plane rather than
written here: `plane.algo` answers `toposort` and `cycles`, each over one named edge type, from
[#98](https://github.com/wangyingsm/dr-strange/issues/98) and
[#99](https://github.com/wangyingsm/dr-strange/issues/99). The order comes back with a generation
per node, so a generation is what may run at once.
- A capped loop-back in a run graph, which is the one cycle worth permitting. A node unsatisfied
with what a predecessor gave it answers something like `{"loop_back": "survey"}`, the flow reruns
that node with the extra context, and a predefined loop count stops it. The loop target is
*declared*, as an edge of its own kind, so a graph that names a node it has not got is refused when
it is written rather than when it is run, and a person reading the graph can see where it may
double back; whether the loop is taken is still the answering node's decision, and only up to the
cap. Two things follow for the ordinary edges: they are labelled `FEEDS` from the first version, so
a later loop-back label is a new label and not a migration of every plane, and both the run order
and the acyclicity check are asked for over `FEEDS` alone — a graph is a DAG in its feeding edges
while its loop-back edges are expected to close cycles.

### Persistence beyond configuration

- Facts and chat session history as long term memory.
- Knowledge and RAG with a graph run.

### Web UI

- Login, the management menu (tenants, users, quota, authorities, LLMs, MCPs, tools, account) and
the gateway menu (rules, processors, agents, graph run, observe), plus a dashboard. Waits on the
management api.

### Money, what is left of it

- Alerting before a cap is reached: a soft threshold that notifies rather than refuses. Needs a
delivery mechanism the gateway has none of, which is the whole of why it is not built.
- A per-tenant price or markup. A price today is what the vendor charges this deployment, and
what a tenant is billed is left outside. Carrying it here means a second table and a decision
about which of the two a usage row should hold.

### Observability, what is left of it

- The usage row still keeps the redaction kinds as a formatted string. The dashboard reads
them apart now, but a query over the rows — *what did this tenant redact last month* — still
has to read a string the wrong way round. Turning the column structured is a migration on both
backends, the codec and the api, and is worth doing the day somebody asks that question of the
ledger rather than of a graph.
- Nothing filters a log line. See the caveat above.

### Security, what is left of it

- Optional eBPF rate limiting per source address, Linux only.
- A tenant managing its own filter rules through the management api, stored rather than
configured, scoped the way a plugin rule is. The floor it adds to would be unchanged.
- Scanning a body that is not json, and one too large to hold. Both are reported as unscanned
today, which is honest and is not the same as covered.
