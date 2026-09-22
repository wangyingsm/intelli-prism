# An AI based Model/Agent Proxy/Gateway

## Key design points

- LLM proxy: the application provides multiple LLM upstream access. network layers includes
	1. Transport Layer - TCP/UDP
	2. Application Layer - HTTP 1.1/HTTP 2/QUIC
	3. Message Layer - OpenAI/Anthropic/Gemini
	4. Data on fly - Response/SSE
- Upstream proxy: upstream maybe a local or remote normal service. same network layers as LLM proxy.
- Downstream proxy: downstream clients maybe a user or an agent. same network layers as upstream.
- Cluster Proxing: a group deployment of this application will serve as one service in a high throughput environment.
- State Sharing: the cluster nodes share their states(eg, cache, configs).
- Long Term Memory: a long term user/agent persist memory with a vector/NLP based memory retrieval mechanism.
- Unified chat endpoint: all users can access this endpoint with any simple interface.
- Unified agent endpoint: all agents can connect to this endpoint with hybrid purposes.
- Intelligent Routing: A SMART mode to choose an appropriate upstream LLM according to their capabilities and the difficulty of the request.
- Graph Agent Run: application has its own agent system, which supports to run agent loop,  subagents, orchestration, cooperation through a graph system.
- Tenants and Users: enterprise level AI proxy/gateway, with sophistic authentication/authorization.
- Quotation and Audition: manage tenants/users tokens' quotation, audit the usages.
- Observation and Tracing: all logs/tracing data both upstream/downstream, performance analysis.
- Web UI: admin for system configuration, manage for tenants/users management interface.

## Primary Goals

- Provide an enterprise/multi-enterprise AI enabled proxy/gateway.
- Support remote/local upstream service nodes. LLM/API/Websocket/GRPC protocols.
- Support multi agent run on server environments to ease the normal/standard development of client agents.
- Provide extra cache level in service so that increase the hit rate and lower token usage.
- Provide persistence of long term memory and knowledge base to provide more precise RAG and quicker AI solutions.
- Provide tenant and/or user separation of their data/memories/knowledges/DAGs, etc.
- Provide smart enough routing strategies to choose one or more upstream LLM(s) for different kinds of problems.
- Provide a DAG based hedging LLM request strategy, then aggregate/select/loop to the final result. then the select path
can be used to self-learn a more efficient router above, with a criterion composed by several factors, including qualities
of the result, token usages, respond time, user bias(scores).
- Provide a tenant/user management of quotas/rate limits/DAGs/audition rules.
- Provide full observations of resources/LLM usages/logs/performance from all levels.
- Async IO every place, including system agents.
- High throughput with a extra low latency.
- Security is the first-class objective.

## Architecture

### Persistence

#### Facts and Chat Session History

- Heavy workload(fast path): a postgresql backed long term memory storage, it store the Facts/Concepts extracted and chat
history and/or agent toolcalls, with embedding and reversed indexing.
- Heavy workload(slow path): a memvid files in object storage(cloud S3/rustfs) backed, same stored data as fast path.
- Heavy workload(hybrid path): with a hot data(access timestamp expiration) through fast path, with a slow path(expired data)
through slow path.
- Light workload/standalone deployment: a sqlite backed, same stored data.
- Feature flag gated compilation: `standalone-storage` compiles in the sqlite backend and `fast-storage` the
postgres one; `hybrid-storage` will add `slow-storage` beside `fast-storage`. The features are additive, so one
build may carry several backends, and the `[storage]` backend tag picks one at startup. A tag naming a backend
the build does not carry stops startup with an error naming the feature to enable.
- A worker thread compaction: when running in hybrid mode, a time interval task to compact fast path to slow path.
- A set of storage traits: expose the capabilities of long term storage to high layers, unified interfaces.

#### Knowledge/RAG and Graph run

- Field knowledges: a dr-strange(https://github.com/wangyingsm/dr-strange-extension) backed graph knowledge storage,
with interfaces can import into it.
- Graph RAG: a mechanism for users/agents to quickly fetch the Field knowledges to enhance LLM generation.
- Graph directed agent run: still `drsg` storage of run graph, automatically deal with dependencies/subagents spawn/inner run loop,
etc.

#### Configuration

- System Configs: including multiple LLM API URL/key/model/temperature(default) data, HTTP service configs, storage/cache configs,
logs, opentelemetry, performance collection, JWT relatives, etc.
- Storage system(cluster): S3/rustfs or other compatibles.
- Storage system(standalone): local toml file.
- The configuration file path is taken from the command line. The current argument handling is hand
rolled in the binary; refactor it to the `clap` crate before the CLI grows past that one positional path.

#### Business data

- Tenants/Users: Argon2id protected passphase, capabilities enabled authorization, all disabled by default except explicit enable.
- Quotation/rate limit: multiple levels quota setting and rate limit setting.
- Routing rules: rules setup/self-learn by tenants/users/system/agents to route the request to upstream endpoints.
- Data above stored in postgresql(cluster) or sqlite(standalone).
- Logs/Metrics: access request logs from downstream and access response logs from upstream. performance metrics collected from
service nodes.
- System agent run loop: run loop logs with both system agent side and LLM side, including LLM thinks if enabled.
- Data above stored two in openobserve.

#### Storage backends

- Each backend writes idiomatic SQL for its own engine and keeps its own migrations (`migrations/sqlite`,
`migrations/postgres`); the storage traits are the only surface the backends share.
- Sqlite spellings postgres does not accept, and what the postgres schema uses instead:
	- `INTEGER PRIMARY KEY` assigns row ids only in sqlite; postgres needs `BIGINT GENERATED ALWAYS AS IDENTITY`.
	- `BLOB` is `BYTEA`.
	- Every integer column is `BIGINT`, since the code binds and reads `i64`. A postgres `INTEGER` is 32 bits: it
	rejects those binds, decodes as `i32`, and a unix timestamp stored in it overflows in 2038 with no test failing.
	- A computed integer needs a cast as well: `length(wasm)` returns a 32 bit `INTEGER` in postgres, so its
	queries select `length(wasm)::BIGINT AS size`.
	- `IFNULL` in an index expression is `COALESCE`, parenthesised as postgres requires.
	- Sqlite's `IS ?` for a null safe comparison is `IS NOT DISTINCT FROM $n`, and every placeholder is numbered.
- Postgres code compiles only with `fast-storage`, and its tests run only when `DATABASE_URL` names a database;
without it they return early and pass having checked nothing, so a plain `cargo test` needs neither. Each test
works in a schema of its own, dropped with its store, so the database may be any one the user can create
schemas in. `docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=ip postgres:16-alpine` provides one, and
`DATABASE_URL=postgres://postgres:ip@localhost/postgres cargo test -p ip-storage --features fast-storage` runs
them. The coverage gate counts them only when run that way.

### Tenants and Users

#### Tenants

- Tenants can be any organization form, eg, an enterprise, a department, a group, etc. create a tenant will always create
a TO(Tenant Owner) user account(Tenant Owner). Tenant is not an entity account. TO is its representative account for management.
- Tenant has its tn_id which is unique in the application, a random 16 bytes key is generate to tenant, named tn_key. it can be represent
as a 32 chars hex string and become a root key of the whole tenant roles.

#### Users

- Users can be created by system admin or any TO, but it is not belong to just one tenant. so user <-> tenant is a many-to-many
relation.
- User has its user_id which is unique in the application, user will get a (user, tenant) combined key for every tenant it belongs
to, named ut_key. compute by sha256(concat(user_id, tn_key)).

### Authentication and Authorization

#### Authentication

- JWT for web.
- Authorization of API call:
	- A customer header `X-Ip-Tnid` is used for identify tenant ID.
	- A customer header `X-Ip-Userid` is used for identify user ID.
 	- A customer header `X-Ip-Signature` is used as the request identity.
	- A customer header `X-Ip-Nonce` is used to prevent request replay.
	- `X-Ip-Signature = sha256(concat(ut_key, nonce))`.
	- For server side, check user exist, fetch tn_key by `X-Ip-Tnid`, then compute
	```
		sign = sha256(concat(sha256(concat(user_id, tnkey)), nonce))
	```
	compare sign with header `X-Ip-Signature` byte by byte as verification.
	- System administrator can NOT make any API call except the call is made from localhost.
	- TO can authenticate with `sign = sha256(concat(tn_key, nonce))` to API calls, but it is not recommended since
	TO is designed to be a tenant management account.

#### Capabilities

- System capabilities enum(TenantMgr, UserMgr, ApiAccess, ApiAdvMgr, LimitMgr, SysAgent, Observer).
- TenantMgr is based on a tuple of (user_id).
- UserMgr is based on a tuple of (user_id, tn_id).
- ApiAccess, ApiAdvMgr and LimitMgr are based on a tuple of (user_id, tn_id, Api_id).
- ApiAdvMgr and LimitMgr are inheritated from ApiAccess, that is no ApiAdvMgr or LimitMgr exists if ApiAccess is disabled.
- SysAgent, Observer are based on a tuple of (user_id, tn_id) and additional ApiAccess if needed.
- SysAdmin is a special role with all capabilities only assign to system administrator account.
- TenantAdmin is a special role with all tenant's capabilities only assign to TO by SysAdmin.

#### Authorization

- SysAdmin is authed to all capabilities on web and API call through localhost.
- Tenant's API range is granted by SysAdmin, quota/limitation of tenant level is setup by SysAdmin too.
- TO is authed to UserMgr, ApiAccess, ApiAdvMgr, LimitMgr, SysAgent, Observer within its tenant range.
- User's ApiAccess, ApiAdvMgr, SysAgent is granted by TO within its tenant range. and TO set every users' quota/limitation
within its tenant.
- User can choose an active tenant in web which the tenant is attached to the user.
- User can never change its capabilities, they are all set by TO.

### Proxy/Gateway

#### Dataflows

- State machine flow definition: receive(kernel side) -> authentication -> header read -> authorization -> header processor(plugin chain)
-> body read -> body processor(plugin chain) -> route(send to upstream) -> response header read -> response header processor(plugin chain)
-> response body read -> response body processor(plugin chain) -> send(kernel side)
- Authorization runs before any plugin, so a request the caller may not make never reaches tenant code and spends no plugin fuel.
Resolving the route is part of authorization, because both the capability check and the plugin rules are scoped to the api the route
names; it also means no header plugin can steer a request somewhere else.
- Every errors in the flow should return gateway error to client(exception - may retry where network IO failed, then wait for
final result).
- When a rule has no request or response body plugin processor, body can be just passed from fan-in to fan-out with a zero-copy mode.
- SSE can have processors too, they will be a different plugin from normal body response. do the chunk processing jobs.
- A response chunk plugin sees one server sent event at a time. The gateway buffers a `text/event-stream`
response up to each blank line, runs the chunk chain on that event and sends it on; every other response
goes through the response body chain instead. A chunk plugin that refuses mid-stream cannot change a
status that has already gone out, so the caller gets a final `event: error` carrying the reason and the
stream ends. A chunk plugin that fails ends the stream, and its reason stays in the log.
- Websocket and gRPC do not support processors for now.

#### Plugins

- Plugins are WASM files, system provides some primary plugins, such as authing, logging, metrics sampling, quota/rate limit blocking,
etc. tenants/users can upload their own UDW(User defined WASM) plugins, UDW can not use any WASI. they only do a input to output string
transformation. and UDW has instruction and time run limit.
- A call takes its instance from a pool reserved when the host starts, so no request pays to map and
unmap guest memory. The pool holds room for 1000 calls at once, which covers the blocking pool every
plugin call runs on; a call that finds the pool exhausted fails rather than waits. Each slot is capped
at the per call memory limit, and what the pool reserves is address space, not resident memory. A
plugin that declares more memory than that cap is refused when it is loaded rather than when it is
first called, so an oversized global plugin stops startup and an oversized tenant one is dropped.
- Each slot reserves exactly the memory limit plus a 32 MiB guard, about 94 GiB for the whole pool.
Left at wasmtime's 4 GiB default a slot would reserve nearly 4 TiB per pool, and a process holding a
few dozen hosts, as the test suite does, runs out of address space. The price is that guest memory
accesses carry explicit bounds checks, which a 4 GiB reservation lets wasmtime leave out; memory
protection keys could win them back on CPUs that have them. (todo)
- On one developer laptop a whole call — instantiate, transform, drop — costs about 5.8us from the
pool against about 13us without it, so the pool saves roughly 7us of every call. `cargo run --release
-p ip-plugin --example plugin_cost` times both in a single process, alternating between them, because
this machine's clock settles differently in each process by more than the difference being measured.
- A slot keeps none of its guest memory resident between calls, so every call faults its pages in again
as fresh zeroed memory. Keeping some resident would save that, but each warm slot holds its share and up
to 100 slots stay warm: at the 64 MiB cap that is 6.4 GiB resident. Size what stays resident from the
memory real plugins touch, and bound the warm slots to match, once there are plugins to measure. The
pool's roughly 94 GiB of reserved address space must also be checked against address space limits and
strict overcommit settings before a deployment relies on it. (todo)
- Plugins are stored in Obj Stor engine(cloud S3/rustfs). they are loaded and compiled at startup or reload of the application.
keyed by WASM file sha256 checksum. plugin types enum(ReqHeader, ReqBody, RespHeader, RespBody, RespChunk), and order as a u8 integer.
- One request can be parsed an explicit tenant and an explicit user, then can load the plugin rule records from cache. those records
form a sequential chain of plugins. in the way of high rule order prior to low order. so order should be unique in storage.
- order defined in range [0, 63] is reserved for primary plugins, so maximum 192 UDW is allowed. so the primary plugins is managed
by admin only, they are applied to every flow by default but with low priority. 

#### Routing

- Routing rule's key is tuple of (protocol, host, port, abs path), maps to a value that is a non empty
list of (protocol, host, port, abs_path) tuples. One entry is the common case for a remote upstream;
several stand behind a replicated or local one.
- Which entry a request goes to is a dispatch decision, not a property of the rule: round robin, least
load, hedging and the rest are chosen per rule. Until those algorithms are designed the first entry is
used. (todo)
- Protocol is an enum (`http://`, `https://`, `ws://`, `wss://`, `tcp`), which `tcp` is for gRPC only.
- Host is a network hostname or IP addr(v4 and v6).
- Port is a u16 integer.
- Abs path is a network slash separated string, which is the method name in gRPC.
- Rules come from two sources and both are live at once:
	- Static: the LLM upstreams declared in the configuration file, loaded at startup and on reload.
	- Dynamic: extra API gateway routes held in the database, added or changed at runtime with no restart.
- The router resolves against the union of the two sources, so a lookup never needs to know which one a rule came from.
- Config wins: when a static and a dynamic rule share the same key, the configured one is served and the database one is ignored, so a runtime write can never subvert a deployed route.
- Protocol, host and port match exactly. The abs path matches by longest prefix on segment boundaries,
and the unmatched remainder is appended to the target's abs path, so one rule covers a whole upstream
api. An exact tuple match is the case where that remainder is empty.
- A configured upstream derives its key from the server's listen address with the upstream id as the
path root. An `[upstream.route]` block overrides any part of that key, which is what a gateway behind
a proxy or serving several hostnames needs.
- The gateway keeps the `/_ip` path prefix for its own endpoints. A rule whose key path lies inside it
is refused when the routing table is built, so no rule can shadow health or identity.
- The table and the plugin chains are held behind one pointer and replaced together, so a reload never
routes a request by one set of rules and processes it by another. Readers never wait for the writer: on
one developer laptop, with 1000 rules and 8 threads resolving, a writer swapping every 200us (about
5000 swaps a second, hundreds of times what a reload does) moved the median resolve from 5107ns to
5148ns and left the 99.9th percentile inside the noise. The swap itself costs about 667ns. Building the
replacement costs about 3.5ms for 1000 rules, and compiling wasm costs far more, which is why a reload
builds off the request path and only then swaps. `cargo run --release -p ip-gateway --example
swap_cost` measures it, alternating the quiet and swapping arms each round.
- Lookup is a hash on (protocol, host, port), then a linear scan of that group's rules ordered longest
path first, so it costs O(n) in the size of one authority group. That is acceptable at the tens of
rules a deployment starts with and is not acceptable at a million, where it would cost tens of
milliseconds per request. Replace the scan with a path trie before rule counts grow.

### Cache

#### Backend

- Redis in cluster(default) mode: a cluster of intelli-prism nodes share their caches with redis.
- Sled in standalone mode: a fallback sled local storage for just single node local deployment.
- Cache traits: expose the capabilities of cache to high layers, unified interfaces.
- Feature flag gated compilation, as storage: `standalone-cache` compiles in the sled backend and
`cluster-cache` the redis one. The features are additive, and the `[cache]` backend tag picks one at
startup; a tag naming a backend the build does not carry stops startup with an error naming the feature.
- Sled tests run against a temporary database. Redis tests run only when `REDIS_URL` names a server,
each under a key prefix of its own that goes when its store does, so `docker run --rm -d -p 6379:6379
redis:7-alpine` is all they need. Without the variable they return early, as the postgres tests do.

#### Cache Levels

- Request/Response cache: a cache for request/response pairs, with a TTL and LRU eviction policy. cache key is a sha256 over the route key, the tenant and the request body, so a hit can only ever be the same tenant asking the same route the same thing; the cache value is the response body. A hit answers from the cache and skips the upstream call and every stage after it, so it spends no tokens and runs no response processor. The request header processors, where logging belongs, have already run by then.
- Syntax cache: a HNSW index and embedding vector based cache for semantic understanding of the request body, with a TTL and LRU eviction policy. cache key is a sha256 of the request body embedding. and this is only for LLM API request.
- System cache: other system temporary data state, such as nonces, tn_key, ut_key, etc. with a TTL and LRU eviction policy.

#### Cache Management

- TTL eviction: a REQ/RESP cache entry has its own TTL each, can be set by two levels, the system
default and the response's own expiration headers, with their precedence order from low to high as
above. `Cache-Control: max-age` names the span, `Expires` names the moment, and `no-store`, `no-cache`
or a span already past keeps nothing at all. A rule does not set a TTL of its own: the upstream
answering knows how long its answer is good for, and the system default covers an upstream that says
nothing. all syntax caches and all system caches have their own TTL settings.
- LRU eviction: three cache levels have their own cache size limits, when the cache size exceeds the limit, the LRU eviction policy will be triggered to evict the least recently used cache entry.
- An entry may be written with no ttl at all, and then stays until something removes or evicts it.
That is what a value invalidated only by its own change needs, such as a tenant key in the system cache.
- What is kept at all: only a response the upstream answered successfully, and never an event
stream, which is answered as it arrives rather than held. A cache that cannot be read or written
costs a round trip upstream and is logged, but never fails the request it was serving.
- Cache updates/invalidations: the system cache will be updated/invalidated automatically when their values changed.
- Where a level's size limit binds: the sled backend holds the whole store, so it enforces its own
limits, and only it is configured with them. It weighs each entry as its key plus its value, keeps
each level's total and an index of its keys ordered by last use, and drops from the oldest end until
the level is inside its limit. A read counts as a use, but refreshes that record at most once a
second, so a busy key does not write to the index on every read. Redis evicts by its server wide
`maxmemory-policy` instead, so there the per level limits would be advisory and the TTLs do the real
work.
- The semantic cache waits for its own milestone, since it needs an embedding model and a live LLM
endpoint. The system cache and the request/response cache come first.

### Intelligent Routing and Self Learning

#### Routing Strategies

- Routing strategies are a group of routing methods the system provides, including round robin, least load, hedging, etc. and all rules can select one of them default to round robin.
- Round robin: system keeps a counter for each rule, and each request will be routed to the next upstream in the list by modulo of the counter and the list length.
- Least load: system keeps a load score for each upstream, and each request will be routed to the upstream with the least load score. load score is calculated by a combination of the number of active requests and the average response time of the upstream.
- Ratio dispatch: every upstream has a ratio value, system rolls a random number and dispatch the request to the upstream according to the ratio value. for example, if there are three upstreams with ratio values of 1, 2, 3, then the first upstream will get 1/6 of the requests, the second will get 2/6 of the requests, and the third will get 3/6 of the requests.
- Hedging: system keeps an additional hedging counts `N` of every rule, with one of the routing strategies above, system will send the request to `N` upstreams in parallel, and return the first response to the downstream. the other async reqeusts will be cancelled. this can reduce the latency of the request.

#### LLM Endpoints Chosen by the Difficulty of the Request

- Difficulty classes: when requests to LLM come in, the system will classify them into different difficulty classes {`easy`, `routine`, `median`, `hard`, `research`}. `easy` is a common-sense question for simple chat with upstream LLM. `routine` is a question driven by agents with simple tool calls, the agent-loop should expect within no more than 3 turns. `median` is a simple task request driven by agents, eg, a simple new function of a code project, a translation of a paper, etc. `hard` is a complex task request driven by agents, eg, a new feature of a code project, a new algorithm design, etc. `research` is a research level task request driven by agents, eg, a thorough analysis of a large codebase, a new research paper writing, etc.
- LLM endpoint selection: every upstream LLM endpoint has its capabilities, it is defined by (provider, model, optional version or release date), with its corresponding difficulty classes it can handle. when a request comes in, the system will classify the request into a difficulty class, and then select a upstream LLM endpoint which is capable of handling the request and with the lowest capabilities, unless the request specifically requires one. in a case that no upstream LLM endpoint can handle the request, the system will dispatch the request to the most capable upstream LLM endpoint available.
- Arbitor of difficulty classification: 2 levels of difficulty classification, if a request is pretty sure that it can be classified by a decision tree. eg, `need tool calls`, `may need several turns`, `need coding`, etc. the leaves of the decision tree should be a semantic similarity leads to final class. these are totally run locally within the system itself, named level 1. if the decision search final results into a less confidence score, say 50%. then a external LLM endpoint should be asked for. let the LLM to make the final decision.
- Chat turn sticky: system groups the requests from a same user session into a chat turn, and records the unique chat turn ID together with the difficulty class. system should keep dispatching the requests from a same chat turn to the same upstream LLM endpoint. so only one difficulty classification is needed for one chat turn.

#### Self Learning

- If a request's difficulty classification is made by an external LLM arbitor, we should also ask the LLM to provide a summary of the request and a short reasoning of the classification. then we can insert a new leaf node into the decision tree with the summary and reasoning, so that next time a similar request comes in, we can classify it locally.
- Best effort run: a special system endpoint to provide a best effort run of a request/chat turn. the request to this endpoint will not try to classify the difficulty, but sending the request to all available upstream LLM endpoints. then wait for all responses. two strategies to respond to downstream user {`aggr` for aggregation, `one` for choosing the best}, which can be aggregated or chosen by the arbitor LLM. if `one` is set, the choice can stored into the decision tree as a new leaf node, so that next time a similar request comes in, we can classify it locally.

### System Agents and Run Graph

#### Agent Template

- All system agents/subagents can be defined by editing a template. so a(n) agent/subagent can be initialized by this template. a well setup template is stored in postgresql or sqlite, and can be updated by system admin or TO. the template is a DB record with the following fields:
  - agent_id: a unique id for the agent/subagent, auto-generated.
  - agent_name: a name for the agent/subagent.
  - sandbox: a json object for specifying the virtual environment of the agent/subagent running. it is a file tree structure composed of relative path as key and object storage file path as its value. eg,
```json
{
  "root": {
    "bin": {
      "python": "s3://mybucket/bin/python",
      "bash": "s3://mybucket/bin/bash"
    },
    "lib": {
      "libc.so.6": "s3://mybucket/lib/libc.so.6",
      "libm.so.6": "s3://mybucket/lib/libm.so.6"
    },
    "usr": {
      "local": {
        "bin": {
          "mytool": "s3://mybucket/usr/local/bin/mytool"
        }
      }
    }
  }  
}
```
  - agent_config: a json object for the agent/subagent's configuration. 
- The json config contains:
  - role: a string for the agent/subagent's role, eg, `system`, `user`, etc.
  - propmt: a string for the agent/subagent's prompt, eg, `You are a helpful assistant.`, etc.
  - tool_set: a list of system tools the agent/subagent can use, eg, [`python`, `bash`, `mytool`], etc.
  - max_turns: an integer for the maximum number of turns the agent/subagent can take in a conversation, eg, 10, etc.
  - time_limit: an integer for the maximum time limit in seconds the agent/subagent can run, eg, 60, etc.
  - req_timeout: an integer for the maximum request timeout in seconds the agent/subagent can wait for a response from upstream LLM, eg, 10, etc.
  - use_best_effort: a boolean for whether the agent/subagent should use the best effort run endpoint, eg, true, false.
  - best_effort_strategy: an optional string of the best effort run strategy, eg, `aggr`, `one`.
- The initialization of the template needs a init_chat_message string and an optional extra_prompt string. the latter is to provide additional messages for the LLM to a better understanding of the context. eg, to privide the data schema of a DB additional to system SQL tools.

#### Sandboxing

- Ip-VFS: all system agents/subagents should NEVER modify any files/data in the host. we manage our the files needed in a virtual file system, named ip-vfs. the ip-vfs' files are stored in system object storage. but all system tools operate them the same way as the host file system. eg, `cat /data/README.md` will actually read the corresponding file in the object storage.
- Any mutation of the virtual file will create a new version of the file in the object storage. 
- System should provide `file(s)/directory(s) recursive` sync up/down between user and ip-vfs. a CLI tool maybe more suitable for this purpose than a web UI. 

#### Networking and Buffering

- All requests from system agents/subagents should go through the system proxy/gateway, so that the requests can be authenticated, authorized, processors transform. and the requests can be routed to the appropriate upstream LLM endpoints. and the requests can be logged and monitored.
- Since the agent loop runs on the same host, and the requests are send to localhost, so the response from the LLM can be move directly to the agent/subagent without copy overhead.

#### System Tools

- System tools are a preset of tools, eg, core-utils, awk, sed, grep, etc.
- 3 scripting languages: python, js/ts, bash. they are all sandboxed in the ip-vfs.
- Optional integrated MCP tools, eg, `sql` with read-only access to DB.

#### Run Graph

- A `dr-strange` stored graph plane to run multiple agents/subagents in a DAG mode. each node in the plane is a agent initialization setup. with the `agent_id` property as the DB key. 
- Each edge between 2 nodes are dependency order of the 2 agents/subagents. the input of the dependentee is the output of the dependenter.
- Invoking a graph run is just a API endpoint call, say `POST /_ip/graph_run/1` with init_message and option extra_prompt to the first node(main agent) of the graph.

### Web UI

#### Login

All unauthenticated page request will redirect here. successful login will setup a JWT and stored in cookies.

#### Management

- System management menu are on the top right of every page.
- Menu items: `Tenants`(A), `Users`(A/TO), `Quota`(A/TO), `Authorities`(A/TO), `LLMs`(A), `MCPs`(A), `Tools`(A), `Account`(A/TO/U), A for system admin, TO for tenant owner, U for normal user.
- `Tenants` is the managing page for system admin to create/disable/edit tenants.
- `Users` is the managing page for A/TO to create/disable/edit users per the user data scope.
- `Quota` is the managing page for A/TO to setup tenants/users token quotation and rate limits.
- `LLMs` is the config page for managing the system global LLM settings.
- `MCPs` is the config page for managing the system global MCP settings providing system agent usage.
- `Tools` is the config page for managing the system global tools setting prviding system agent usage.
- `Accounts` is the user account drop down menu for password changing, tenant application, log out.

#### Gateway Functions

- Gateway functions menu are on the left bar of every page.
- Menu items: `Rules`(A/TO), `Processors`(A/TO), `Agents`(A/TO), `GraphRun`(A/TO), `Observe`(A/TO/U).
- `Rules` is the managing page for A/TO to setup API mappings.
- `Processors` is the managing page for A/TO to create/disable/update header or body processors.
- `Agents` is the managing page for A/TO to create/disable/edit the system agent templates.
- `GraphRun` is the managing page for A/TO to create/disable/edit the dr-strange graph plane for multiple agents flow.
- `Observe` is the logging or statistics view page, logging view can link to external open-observe page.

#### Dashboard

Home page of the web UI. showing important latest system(cluster) stats in one page.

### Observability

- All logs will send to open-observe in normal/cluster mode. logs write to local file in standalone mode.
- System agents' chat histories are all stored in system's long term memory storage. can be viewed by authorized users.
- Every request/response can be traced with a unique trace ID. and also at least 4 spans there: ingress request, egress request, ingress response, egress response.
- All requests/responses within one LLM chat turn(agent run loop) should be grouped into one turn, keyed by turn_id.
- Open-telemetry compatible, can output these data to any supported platforms.

### Security

- Any tenants/users data SHOULD be strictly isolated.
- All agents SHOULD run in a sandbox.
- System provide builtin security processors to filter out any sensitive data in requests and/or responses to/from upstreams. eg, keys of any kind, passphase of any kind, personal sensitives, contacts, etc.
- A optional DDOS proof network layer to limit TCP packages rate from one IP. installed as a XPath eBPF filter(only Linux).
