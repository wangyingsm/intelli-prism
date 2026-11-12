#!/usr/bin/env bash
# One graph run across three members of a cluster, against a real model.
#
#   examples/cluster-run/run.sh [output directory]
#
# Needs `IP_API_KEY`, `drsg` on the path, and a postgres and a redis to point at. The three
# gateways, the plane and the schema are made here and go when the run does; the two
# containers are yours and are left alone but for one database created and dropped.
#
#   storage ─┐
#   cache ───┼─ generation 0        editor   generation 1
#   limits ──┘
#
# What a single process cannot show, and the reason this example exists beside
# `examples/graph-run`:
#
#   * Nodes landing on members that are not the one the run was started on. The
#     coordinator is a candidate for its own work like any other member, so with three
#     members and round-robin a four-node graph lands roughly one node in three here.
#   * The record of where each node is running, written to the plane by the coordinator.
#     It is sampled *while the run is going*, not read at the end: the record says where a
#     node *is*, and the coordinator clears it the moment that node ends.
#   * The events coming home. A dispatched node's turns reach the caller's own stream
#     indistinguishably from a local node's, which is the whole point of the dispatch.
#   * The answers kept in the shared cache under (run, node), so a second coordinator
#     picking the run up would not pay for those nodes again.
#
# Not shown: a stop crossing to a node already running elsewhere. It needs a node to fail
# while a sibling is mid-turn, which is a race to arrange and a certainty to assert — so it
# is tested rather than demonstrated. See `a_stop_the_coordinator_published_ends_a_node_running_here`
# in `src/routes/cluster.rs`.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="${1:-$here/out}"

# The three members. Alpha is the one the run is started on, so alpha coordinates.
alpha_port="${IP_ALPHA_PORT:-18100}"
beta_port="${IP_BETA_PORT:-18101}"
gamma_port="${IP_GAMMA_PORT:-18102}"
drsg_port="${IP_DRSG_PORT:-18109}"

# Yours. The defaults are the containers this repository's test gate uses.
postgres="${IP_POSTGRES_URL:-postgres://postgres:ip@127.0.0.1:55432/postgres}"
redis="${IP_REDIS_URL:-redis://127.0.0.1:56379}"

base_url="${IP_BASE_URL:-https://api.deepseek.com/v1}"
model="${IP_MODEL:-deepseek-chat}"
dialect="${IP_DIALECT:-openai}"
input="${IP_INPUT:-What does an enterprise AI gateway need?}"

pass="correct horse staple"
host="http://127.0.0.1:$alpha_port"
work="$(mktemp -d)"
jar="$work/cookies"
token="drsg-$(date +%s)-$$"
# The whole of what authenticates one member to another. Generated per run, never reused,
# and written only into files this user can read.
secret="cluster-$(head -c 24 /dev/urandom | base64 | tr -d '/+=')"
db="ip_cluster_example_$$"
store=""
watcher=""
members=()

say() { printf '\n== %s\n' "$*"; }
cleanup() {
	for pid in "${members[@]}" "$store" "$watcher"; do
		if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
			kill "$pid" 2>/dev/null || true
			wait "$pid" 2>/dev/null || true
		fi
	done
	# The database this run made, and nothing else in that server.
	psql "$postgres" -q -c "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1 || true
	rm -rf "$work"
}
trap cleanup EXIT

[ -n "${IP_API_KEY:-}" ] || {
	echo "set IP_API_KEY to a key for $base_url" >&2
	exit 1
}
for tool in drsg psql redis-cli python3; do
	command -v "$tool" >/dev/null || {
		echo "$tool is not on the path" >&2
		exit 1
	}
done
psql "$postgres" -q -c 'SELECT 1' >/dev/null 2>&1 || {
	echo "no postgres at $postgres — set IP_POSTGRES_URL" >&2
	echo "a cluster shares one store; that is what makes three processes one deployment" >&2
	exit 1
}
redis-cli -u "$redis" ping >/dev/null 2>&1 || {
	echo "no redis at $redis — set IP_REDIS_URL" >&2
	echo "a cluster shares one cache; the stop signal and the kept answers travel through it" >&2
	exit 1
}
mkdir -p "$out"

say "building the cluster server (postgres and redis, the default features)"
cd "$root" || exit 1
cargo build --quiet || exit 1
binary="$root/target/debug/intelli-prism"

say "a database of this run's own, so the run touches nothing else in that server"
psql "$postgres" -q -c "CREATE DATABASE $db" >/dev/null || exit 1
# The same url with the database swapped, which is what every member will be given.
shared="${postgres%/*}/$db"

say "the plane: the graphs, the members, and where each node ran"
DRSG_TOKEN="$token" drsg serve --addr "127.0.0.1:$drsg_port" -d "$work/graphs.drsg" \
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
		echo "the plane stopped before it listened:" >&2
		cat "$work/drsg.log" >&2
		exit 1
	}
	sleep 0.1
done
rpc plane.create '{"name":"ip_graphs"}' >/dev/null

say "three configurations differing in two keys: what each binds and what it advertises"
# The key and the cluster secret reach each member in a file only this user can read.
umask 177
for member in alpha:$alpha_port beta:$beta_port gamma:$gamma_port; do
	name="${member%%:*}"
	port="${member##*:}"
	cat >"$work/$name.toml" <<-TOML
		[server]
		listen = "127.0.0.1:$port"

		# Everything below this line is identical on all three, by definition: a cluster is
		# machines that share a store, a cache and a plane. Only the two lines above differ,
		# plus the advertise below, which is this member saying where it answers.
		[cluster]
		advertise = "127.0.0.1:$port"
		heartbeat = 2
		secret = "$secret"
		strategy = "round_robin"

		[storage]
		backend = "postgres"
		url = "$shared"

		[cache]
		backend = "redis"
		url = "$redis"

		[auth.jwt]
		issuer = "intelli-prism"
		secret = "0123456789abcdef0123456789abcdef"

		[graph_store]
		url = "http://127.0.0.1:$drsg_port/rpc"
		token = "$token"
		plane = "ip_graphs"
		timeout = 30

		[[upstream]]
		id = "provider"
		base_url = "$base_url"
		api_key = "$IP_API_KEY"
		model = "$model"
		caller_speaks = "$dialect"
		upstream_speaks = "$dialect"
	TOML
done
umask 022

say "one system administrator, in the store all three share"
printf '%s' "$pass" | "$binary" --config "$work/alpha.toml" admin create root || exit 1

say "the three members, each registering itself in the plane"
for name in alpha beta gamma; do
	"$binary" --config "$work/$name.toml" >"$work/$name.log" 2>&1 &
	members+=("$!")
done
for member in alpha:$alpha_port beta:$beta_port gamma:$gamma_port; do
	name="${member%%:*}"
	port="${member##*:}"
	for _ in $(seq 1 150); do
		curl -sf "http://127.0.0.1:$port/_ip/healthz" >/dev/null 2>&1 && break
		sleep 0.1
	done
	curl -sf "http://127.0.0.1:$port/_ip/healthz" >/dev/null 2>&1 || {
		echo "$name stopped before it listened:" >&2
		tail -40 "$work/$name.log" >&2
		exit 1
	}
done

say "who is in the cluster, as the plane has it"
# Two heartbeats, so a member that registered late is in before the run is started.
sleep 5
rpc plane.query \
	'{"plane":"ip_graphs","plan":{"source":{"ScanLabel":"ClusterMember"},"steps":[]}}' |
	python3 -c 'import json,sys
answer = json.load(sys.stdin).get("result", [])
rows = answer if isinstance(answer, list) else answer.get("rows", [])
for row in rows:
    props = row.get("properties", row)
    print("  ", props.get("address"), "last said it was here at", props.get("at"))'

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
api POST /_ip/tenants "{\"tenant\":\"shop\",\"owner\":\"shopkeeper\",\"passphrase\":\"$pass\"}"

say "the four templates the graph's nodes name"
for node in storage cache limits editor; do
	api PUT "/_ip/agents/$node?tenant=shop" "$(cat "$here/agents/$node.json")" | head -c 120
	echo
done

say "the graph, written on alpha and read back"
api POST /_ip/login "{\"user\":\"shopkeeper\",\"passphrase\":\"$pass\"}"
api PUT "/_ip/graphs/needs?tenant=shop" "$(cat "$here/graph.json")" | head -c 200
api GET "/_ip/graphs/needs?tenant=shop" | head -c 200

# Sampled while the run goes, because a placement is cleared when its node ends: read at
# the end the plane is empty and says — correctly — that nothing is running anywhere.
: >"$out/placements.jsonl"
(
	while :; do
		rpc plane.query \
			'{"plane":"ip_graphs","plan":{"source":{"ScanLabel":"GraphPlacement"},"steps":[]}}' \
			>>"$out/placements.jsonl"
		echo >>"$out/placements.jsonl"
		sleep 0.25
	done
) &
watcher=$!

say "the run, started on alpha (a real model answers four times)"
curl -s -N -b "$jar" -c "$jar" -X POST -H 'content-type: application/json' -H 'x-ip-csrf: 1' \
	-w '\n[status %{http_code}]\n' \
	-d "{\"input\":$(printf '%s' "$input" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')}" \
	"$host/_ip/graph_run/needs?tenant=shop" >"$out/stream.sse"

kill "$watcher" 2>/dev/null || true
wait "$watcher" 2>/dev/null || true

say "the answers kept in the shared cache, under the run and the node"
# Scoped to this run's own id rather than to every graph run the server has ever held:
# the cache is shared with whatever else uses that redis, and a key per run is the point.
ran="$(python3 -c '
import json, sys
for line in open(sys.argv[1], encoding="utf-8"):
    if line.startswith("data:"):
        run = json.loads(line[5:]).get("run")
        if run:
            print(run)
            break
' "$out/stream.sse")"
if [ -n "$ran" ]; then
	redis-cli -u "$redis" --scan --pattern "ip:sys:graph-run/$ran/*" | sort >"$out/kept-keys.txt"
else
	: >"$out/kept-keys.txt"
fi
wc -l <"$out/kept-keys.txt" | tr -d ' ' | xargs printf '  %s keys\n'

for name in alpha beta gamma; do cp "$work/$name.log" "$out/$name.log" 2>/dev/null; done

say "what it said, and who did what"
python3 "$here/report.py" "$out/stream.sse" "$out/placements.jsonl" \
	"127.0.0.1:$alpha_port" "$out/kept-keys.txt"
echo
echo "the whole stream:  $out/stream.sse"
echo "each member's log: $out/{alpha,beta,gamma}.log"
