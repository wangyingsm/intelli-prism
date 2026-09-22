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

## Status

**This is not production software.** It is an unfinished project under active development, at
version 0.1.0, and nothing about it is stable — the configuration format, the storage schema, the
wire headers and the plugin interface all still change without ceremony.

Large parts of the design are unbuilt: there is no management api, no login endpoint, no web ui, no
quota or rate limiting, no agents, no semantic cache, and telemetry is not exported anywhere yet.
The pieces that do exist are tested, but they have never run under real traffic, and no security
review has been done. Read `ROADMAP.md` before assuming a feature is there.

## License

Dual licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you state otherwise, any contribution you deliberately submit for inclusion
in this work shall be dual licensed as above, with no additional terms or conditions.
