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
- Feature flag gated compilation: as hybrid-storage: fast-storage + slow-storage, standalone-storage exclusive.
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
- Lookup is a hash on (protocol, host, port), then a linear scan of that group's rules ordered longest
path first, so it costs O(n) in the size of one authority group. That is acceptable at the tens of
rules a deployment starts with and is not acceptable at a million, where it would cost tens of
milliseconds per request. Replace the scan with a path trie before rule counts grow.

### Cache

(todo)

### Intelligent Routing and Self Learning

(todo)

### System Agents and Run Graph

(todo)

### Web UI

(todo)

### Observability

(todo)

### Security

(todo)
