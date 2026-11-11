#!/usr/bin/env bash
# One run of a five-node graph against a real model, over a real dr-strange plane.
#
#   examples/graph-run/run.sh [output directory]
#
# Needs `IP_API_KEY` in the environment and `drsg` on the path. Everything else — the server,
# its database, its cache and the plane the graph lives in — is made here and goes when the
# run does, so nothing touches a database or a plane anything else is using.
#
#   rust-impl ─┐                  rust-bench ─┐
#              ├ generation 0                 ├─ compare   generation 2
#   go-impl ───┘                  go-bench ───┘  generation 1
#
# The two authors run at once; each bencher waits for its own author; the comparer waits for
# both benchers and is given their answers keyed by node name.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="${1:-$here/out}"

port="${IP_PORT:-18090}"
drsg_port="${IP_DRSG_PORT:-18099}"
base_url="${IP_BASE_URL:-https://api.deepseek.com/v1}"
model="${IP_MODEL:-deepseek-chat}"
dialect="${IP_DIALECT:-openai}"
input="${IP_INPUT:-Write a merge sort algorithm in Rust and in Go, benchmark each, and compare the results.}"

pass="correct horse staple"
host="http://127.0.0.1:$port"
work="$(mktemp -d)"
jar="$work/cookies"
token="drsg-$(date +%s)-$$"
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
	rm -rf "$work"
}
trap cleanup EXIT

[ -n "${IP_API_KEY:-}" ] || {
	echo "set IP_API_KEY to a key for $base_url" >&2
	exit 1
}
command -v drsg >/dev/null || {
	echo "drsg is not on the path; the graph store is a dr-strange plane" >&2
	exit 1
}
mkdir -p "$out"

say "building the standalone server"
cd "$root" || exit 1
cargo build --quiet --no-default-features --features standalone-storage,standalone-cache || exit 1

say "the graph store, on a database of its own"
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
		echo "the graph store stopped before it listened:" >&2
		cat "$work/drsg.log" >&2
		exit 1
	}
	sleep 0.1
done
rpc plane.create '{"name":"ip_graphs"}' >/dev/null

say "the configuration"
# The key reaches the server in a file only this user can read, never on a command line.
umask 177
cat >"$work/intelli-prism.toml" <<TOML
[server]
listen = "127.0.0.1:$port"

[storage]
backend = "sqlite"
path = "$work/graph-run.db"

[cache]
backend = "sled"
path = "$work/graph-run-cache"

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
umask 022

say "one system administrator, the way an operator makes one"
binary="$root/target/debug/intelli-prism"
printf '%s' "$pass" | "$binary" --config "$work/intelli-prism.toml" admin create root || exit 1
"$binary" --config "$work/intelli-prism.toml" >"$work/server.log" 2>&1 &
server=$!
for _ in $(seq 1 100); do
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

say "the five templates the graph's nodes name"
for node in rust-impl go-impl rust-bench go-bench compare; do
	api PUT "/_ip/agents/$node?tenant=bench" "$(cat "$here/agents/$node.json")" | head -c 120
	echo
done

say "the graph, written and read back"
api POST /_ip/login "{\"user\":\"benchy\",\"passphrase\":\"$pass\"}"
api PUT "/_ip/graphs/mergesort?tenant=bench" "$(cat "$here/graph.json")" | head -c 200
api GET "/_ip/graphs/mergesort?tenant=bench" | head -c 200

say "the run (a real model answers five times; this takes minutes)"
curl -s -N -b "$jar" -c "$jar" -X POST -H 'content-type: application/json' -H 'x-ip-csrf: 1' \
	-w '\n[status %{http_code}]\n' -d "{\"input\":$(printf '%s' "$input" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')}" \
	"$host/_ip/graph_run/mergesort?tenant=bench" >"$out/stream.sse"
cp "$work/server.log" "$out/server.log" 2>/dev/null

say "what it said"
python3 "$here/report.py" "$out/stream.sse"
echo
echo "the whole stream: $out/stream.sse"
