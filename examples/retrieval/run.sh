#!/usr/bin/env bash
# Retrieval against a real model, over both backends, on one corpus.
#
#   examples/retrieval/run.sh [output directory]
#
# Needs `IP_API_KEY`, `IP_EMBED_ARTIFACT` and `ORT_DYLIB_PATH` in the environment, and `drsg`,
# `docker` and `uv` on the path. Everything else — the server, its database, its cache, the plane
# the graphs live in, the plane the corpus lives in and the qdrant the same corpus lives in — is
# made here and goes when the run does.
#
#   look ──> answer        three times: over a collection, over a plane, over the collection again
#
# The two graphs are the same two steps and the same two templates. What differs is the tools the
# `look` node names, which it names itself rather than taking from its template:
#
#   graphs/points.json   docs.search                        a collection, and nothing else
#   graphs/joins.json    kb.search, kb.neighbors, kb.rank    a plane, and the joins in it
#
# And the questions are chosen so that the difference shows. The collection is asked something one
# page answers outright. Then both are asked something whose nearest page has been **superseded**,
# where what to answer with is on the page joined to it by `SUPERSEDED_BY` — and the third run is
# that same question put to the collection, which holds the same page and no join from it.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="${1:-$here/out}"

port="${IP_PORT:-18130}"
drsg_port="${IP_DRSG_PORT:-18139}"
qdrant_port="${IP_QDRANT_PORT:-18133}"
qdrant_image="${IP_QDRANT_IMAGE:-qdrant/qdrant:v1.12.4}"
base_url="${IP_BASE_URL:-https://api.deepseek.com/v1}"
model="${IP_MODEL:-deepseek-chat}"
dialect="${IP_DIALECT:-openai}"

# One page answers this, and no model knows what it says.
flat="${IP_FLAT:-The manifest the shipper sent tonight failed its checksum. What do we do, and how long have we got?}"
# The nearest page to this one has been replaced, and the replacement changed every number in it.
joined="${IP_JOINED:-A reefer on the Rotterdam run logged two hours above its set band. What is the response, and how long have we got to tell the consignee?}"

pass="correct horse staple"
host="http://127.0.0.1:$port"
work="$(mktemp -d)"
jar="$work/cookies"
token="drsg-$(date +%s)-$$"
container="ip-retrieval-$$"
server=""
store=""

say() { printf '\n== %s\n' "$*"; }
cleanup() {
	for pid in "$server" "$store"; do
		if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
			kill "$pid" 2>/dev/null || true
			wait "$pid" 2>/dev/null || true
		fi
	done
	docker rm -f "$container" >/dev/null 2>&1 || true
	rm -rf "$work"
}
trap cleanup EXIT

[ -n "${IP_API_KEY:-}" ] || {
	echo "set IP_API_KEY to a key for $base_url" >&2
	exit 1
}
[ -n "${IP_EMBED_ARTIFACT:-}" ] || {
	echo "set IP_EMBED_ARTIFACT: the query of a look-up is embedded by this deployment" >&2
	exit 1
}
[ -n "${ORT_DYLIB_PATH:-}" ] || {
	echo "set ORT_DYLIB_PATH to libonnxruntime, which the embedding artefact is run by" >&2
	exit 1
}
for tool in drsg docker uv; do
	command -v "$tool" >/dev/null || {
		echo "$tool is not on the path" >&2
		exit 1
	}
done
mkdir -p "$out"

say "building the standalone server, with the embedder compiled in"
cd "$root" || exit 1
cargo build --quiet --no-default-features \
	--features standalone-storage,standalone-cache,semantic || exit 1

say "the graph store and the corpus plane, on a database of its own"
DRSG_TOKEN="$token" drsg serve --addr "127.0.0.1:$drsg_port" -d "$work/planes.drsg" \
	>"$work/drsg.log" 2>&1 &
store=$!
rpc() {
	curl -s -m 10 -H "Authorization: Bearer $token" -H 'content-type: application/json' \
		-d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" \
		"http://127.0.0.1:$drsg_port/rpc"
}
for _ in $(seq 1 100); do
	rpc plane.list '{}' >/dev/null 2>&1 && break
	kill -0 "$store" 2>/dev/null || {
		echo "the graph store stopped before it listened:" >&2
		cat "$work/drsg.log" >&2
		exit 1
	}
	sleep 0.1
done
rpc plane.create '{"name":"ip_graphs"}' >/dev/null

say "a qdrant of its own, which goes when the run does"
docker run -d --rm --name "$container" -p "127.0.0.1:$qdrant_port:6333" \
	"$qdrant_image" >/dev/null || exit 1
for _ in $(seq 1 150); do
	curl -sf "http://127.0.0.1:$qdrant_port/readyz" >/dev/null 2>&1 && break
	sleep 0.2
done

say "the corpus, embedded by the artefact the gateway itself loads, into both backends"
uv run --quiet --with onnxruntime --with tokenizers --with numpy \
	"$here/seed.py" \
	--artifact "$IP_EMBED_ARTIFACT" \
	--corpus "$here/corpus.json" \
	--qdrant "http://127.0.0.1:$qdrant_port" --collection handbook \
	--drsg "http://127.0.0.1:$drsg_port/rpc" --token "$token" --plane handbook || exit 1

say "the configuration"
# The key reaches the server in a file only this user can read, never on a command line.
umask 177
cat >"$work/intelli-prism.toml" <<TOML
[server]
listen = "127.0.0.1:$port"

[storage]
backend = "sqlite"
path = "$work/retrieval.db"

[cache]
backend = "sled"
path = "$work/retrieval-cache"

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"

[graph_store]
url = "http://127.0.0.1:$drsg_port/rpc"
token = "$token"
plane = "ip_graphs"
timeout = 30

# The embedder a look-up turns a query into numbers with. There is no other way to configure one:
# the stanza's own purpose is the third cache level, and it comes with it.
#
# **Which is why the nearness is 1.0, and the README's first finding is about this.** A node's turn
# is an ordinary request as far as the gateway is concerned, so it passes the semantic cache like
# any other: at 0.99 this example answered one node with what a different node of a different run
# had said, because the two prompts meant nearly the same thing. At 1.0 only a question identical
# to one already asked is answered that way.
[semantic]
artifact = "$IP_EMBED_ARTIFACT"
nearness = 1.0

[[upstream]]
id = "provider"
base_url = "$base_url"
api_key = "$IP_API_KEY"
model = "$model"
caller_speaks = "$dialect"
upstream_speaks = "$dialect"
TOML
umask 022

say "one system administrator, the way an operator makes one"
binary="$root/target/debug/intelli-prism"
printf '%s' "$pass" | "$binary" --config "$work/intelli-prism.toml" admin create root || exit 1
"$binary" --config "$work/intelli-prism.toml" >"$work/server.log" 2>&1 &
server=$!
for _ in $(seq 1 200); do
	curl -sf "$host/_ip/healthz" >/dev/null 2>&1 && break
	kill -0 "$server" 2>/dev/null || {
		echo "the server stopped before it listened:" >&2
		cat "$work/server.log" >&2
		exit 1
	}
	sleep 0.1
done

api() {
	local method="$1" path="$2" body="${3:-}"
	if [ -n "$body" ]; then
		curl -s -b "$jar" -c "$jar" -X "$method" -H 'content-type: application/json' \
			-H 'x-ip-csrf: 1' -w ' [%{http_code}]' -d "$body" "$host$path"
	else
		curl -s -b "$jar" -c "$jar" -X "$method" -H 'x-ip-csrf: 1' \
			-w ' [%{http_code}]' "$host$path"
	fi
	echo
}

say "a tenant and its owner — a run is charged to a tenant, so the administrator can start none"
api POST /_ip/login "{\"user\":\"root\",\"passphrase\":\"$pass\"}"
api POST /_ip/tenants "{\"tenant\":\"bench\",\"owner\":\"benchy\",\"passphrase\":\"$pass\"}"

say "the two templates the nodes name — neither names a tool, and both nodes do"
for node in look answer; do
	api PUT "/_ip/agents/$node?tenant=bench" "$(cat "$here/agents/$node.json")" | head -c 120
	echo
done

api POST /_ip/login "{\"user\":\"benchy\",\"passphrase\":\"$pass\"}"

# Registered after the corpus is in, and that order matters for the plane: a collection declares
# its width when it is created, while a plane reports none and is measured by reading what it
# holds — so a plane registered empty is accepted with nothing measured against the embedder.
#
# **Two hits each, so that the comparison is one.** Revision 7 of the excursion page is the
# fourth-nearest page to the reefer question, so neither backend answers it to the question as
# asked. It is not therefore out of reach — a model that rephrases finds it in both, which is the
# README's second finding — and `status` is deliberately not declared filterable for the same
# reason: a filter on it would be a third route to the same page, and what is compared here is
# what the join says rather than what it reaches.
say "the two retrievers, each measured against the embedder as it is written"
api PUT "/_ip/retrievers/docs?tenant=bench" "{
  \"name\": \"The handbook, as points\",
  \"endpoint\": \"http://127.0.0.1:$qdrant_port\",
  \"source\": {\"kind\": \"qdrant\", \"collection\": \"handbook\"},
  \"tools\": [\"search\"],
  \"meta\": {
    \"team\": \"Which team owns the page: dockside, reefer, clearance or platform\"
  },
  \"max_hits\": 2
}" | head -c 400
api PUT "/_ip/retrievers/kb?tenant=bench" "{
  \"name\": \"The handbook, as a plane\",
  \"endpoint\": \"http://127.0.0.1:$drsg_port/rpc\",
  \"auth\": {\"kind\": \"key\", \"key\": \"$token\"},
  \"source\": {\"kind\": \"drsg\", \"plane\": \"handbook\", \"label\": \"Page\",
               \"property\": \"embedding\"},
  \"tools\": [\"search\", \"neighbors\", \"rank\"],
  \"max_hits\": 2
}" | head -c 400

say "the two graphs, written and read back — each node's own tools come back with it"
api PUT "/_ip/graphs/points?tenant=bench" "$(cat "$here/graphs/points.json")" | head -c 400
api PUT "/_ip/graphs/joins?tenant=bench" "$(cat "$here/graphs/joins.json")" | head -c 400
api GET "/_ip/graphs/joins?tenant=bench" | head -c 400

run() {
	local graph="$1" label="$2" request="$3"
	curl -s -N -b "$jar" -c "$jar" -X POST -H 'content-type: application/json' -H 'x-ip-csrf: 1' \
		-w '\n[status %{http_code}]\n' \
		-d "{\"input\":$(printf '%s' "$request" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')}" \
		"$host/_ip/graph_run/$graph?tenant=bench" >"$out/$label.sse"
	mkdir -p "$out/$label"
	python3 "$here/report.py" "$out/$label.sse" --answers "$out/$label" |
		tee "$out/$label/report.txt"
}

say "the collection — one search, one page, the answer is on it (a minute or so)"
run points flat "$flat"

say "the plane — the nearest page is superseded, so the answer is one join away"
run joins joined "$joined"

say "the same question of the collection, which holds the same page and no join from it"
run points blind "$joined"

say "which turns the model answered and which the cache did, which is the first finding"
curl -s -b "$jar" -c "$jar" "$host/_ip/usage?tenant=bench&limit=100" |
	python3 -c 'import collections, json, sys
rows = json.load(sys.stdin)
counted = collections.Counter(row["served"] for row in rows)
print("  turns:", len(rows), "".join(f"  {where}: {many}" for where, many in sorted(counted.items())))'

cp "$work/server.log" "$out/server.log" 2>/dev/null
echo
echo "the streams: $out/{flat,joined,blind}.sse"
