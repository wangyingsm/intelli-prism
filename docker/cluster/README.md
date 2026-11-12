# A cluster on one machine

Two gateways, one store, one cache, one plane. It exists for the half of this system a
single process cannot show: a node dispatched to another member, the record of where it
ran, and a stop that has to cross a boundary to reach it. Everything else is quicker to
test with `cargo nextest run`.

```sh
cd docker/cluster
docker compose up --build -d          # the first build is a full release build
printf 'cluster-dev-pass' | docker compose run --rm -T alpha \
  --config /etc/intelli-prism/config.toml admin create root
curl -s localhost:18080/_ip/healthz   # alpha
curl -s localhost:18081/_ip/healthz   # beta
```

Two things about that seed line, both learned by getting them wrong:

- **`--config` is repeated** because `compose run` replaces the `command:` in the file,
  and without it the binary looks for its default path inside the container.
- **The passphrase is piped once, with no trailing newline.** It is read as the whole of
  stdin, so a second line becomes *part of the passphrase* — `printf 'x\nx'` creates an
  account whose passphrase is `x\nx`, the create succeeds, and every login then fails with
  the same 401 a wrong passphrase gives. Use `-it` instead to be asked twice at a prompt.

Both members then answer for the same account, which is the thing worth checking after
any change to the stack:

```sh
curl -sc jar -X POST localhost:18081/_ip/login \
  -H 'content-type: application/json' \
  -d '{"user":"root","passphrase":"cluster-dev-pass"}'
curl -sb jar localhost:18080/_ip/session   # beta's cookie, alpha's answer
```

A session opened on one member and accepted by the other is what "one deployment" means
here: one store, one jwt secret, one cache. `/_ip/whoami` is not the probe for it — that
route takes a signed request, not a session, and answers 401 to a cookie from either.

| | where | why it is here |
|---|---|---|
| `store` | `127.0.0.1:55433` | postgres, shared. Two members sharing a store is what makes them one deployment |
| `cache` | `127.0.0.1:56380` | redis, shared. A nonce spent on one member is spent on both, and from M27 the stop travels this way |
| `plane` | `127.0.0.1:17700` | the dr-strange plane: the run graphs, and which member each node runs on |
| `alpha` | `127.0.0.1:18080` | advertises `alpha:8080` |
| `beta` | `127.0.0.1:18081` | advertises `beta:8080` |

The gate's own containers are on 55432 and 56379 and are left alone: these are different
ports and a different compose project, so a cluster can be up while the tests run.

## Two things to know before it will do anything interesting

**The upstream in `alpha.toml` points at this laptop's own `:11435`** through
`host.docker.internal`, which is where a local model server would answer. Point it at
something real — a provider and a key, or your own server — or runs will start and then
fail at the first turn, which looks like a cluster bug and is not one.

**The plane binary is bind-mounted from this machine**, at `~/.local/bin/drsg` unless
`DRSG_BIN` says otherwise. There is no published dr-strange image yet, and a dev stack
that needs one built first is a dev stack nobody runs. It links only libc, libm and
libgcc, so the debian base runs it unchanged.

## What is deliberately not here

**No secrets management.** The passwords and tokens are in plain text in the files beside
this one. They protect nothing and are worth nothing; a dev stack whose passwords must be
fetched before it starts is one nobody starts. Do not copy these files into anything that
serves a real request — the jwt secret is a signing key, and the cluster secret is the
whole of what one member checks before running work another sends it.

**No TLS between members.** Inside one compose network there is nothing to protect
against, and from M27 the cluster secret is what authenticates a dispatch. A real
deployment puts the members behind something that terminates TLS.

**No third member.** Two is enough to make a boundary; a third only makes the round-robin
visible, and is one `beta.toml` copy away when it is wanted.
