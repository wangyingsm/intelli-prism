#!/usr/bin/env bash
# Three runs of a graph that branches, against a real model, over a real dr-strange plane.
#
#   examples/branch/run.sh [output directory]
#
# Needs `IP_API_KEY` in the environment and `drsg` on the path. Everything else — the server,
# its database, its cache and the plane the graph lives in — is made here and goes when the
# run does, so nothing touches a database or a plane anything else is using.
#
#            ┌── when users are still affected, or the cause is unknown ──> escalate
#   triage ──┼── when the incident is over and its cause is understood ───> close
#            └── when it is still going on but affects nobody ───────────> shelve
#
# The same graph is run three times, on three incident reports, because one run proves nothing:
# a model that always named the first branch would look exactly like a model that judged. Three
# reports that differ only in what the conditions ask about are what shows a condition being
# read. The third one is also the remedy for what this example found — see the README.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="${1:-$here/out}"

port="${IP_PORT:-18110}"
drsg_port="${IP_DRSG_PORT:-18119}"
base_url="${IP_BASE_URL:-https://api.deepseek.com/v1}"
model="${IP_MODEL:-deepseek-chat}"
dialect="${IP_DIALECT:-openai}"

# An incident still going on, with no cause yet: the escalate condition holds and the other does not.
ongoing="${IP_ONGOING:-Checkout has been returning 503 for roughly 40% of requests since 02:10 UTC. Restarting the payment gateway pods changed nothing. We do not know what is wrong. Support has seven tickets open in the last twenty minutes.}"
# One that is over and understood: the close condition holds and neither of the others does.
settled="${IP_SETTLED:-Image uploads failed for 18 minutes from 14:02 to 14:20 UTC. A deploy at 14:01 shipped a config with the wrong S3 bucket name; rolling it back fixed uploads immediately and every upload since has succeeded. About 300 users saw an error toast.}"
# Still going on, cause known, nobody affected: only the shelve condition holds.
quiet="${IP_QUIET:-A background reindex job has been failing every ten minutes since 08:00 UTC, because its service account lost permission in last night key rotation. It is still failing now. No user-facing page reads that index yet, so nobody is affected and nobody has complained.}"

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
path = "$work/branch.db"

[cache]
backend = "sled"
path = "$work/branch-cache"

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

say "the four templates the graph's nodes name"
for node in triage escalate close shelve; do
	api PUT "/_ip/agents/$node?tenant=bench" "$(cat "$here/agents/$node.json")" | head -c 120
	echo
done

say "the graph, written and read back — each branch comes back with its condition"
api POST /_ip/login "{\"user\":\"benchy\",\"passphrase\":\"$pass\"}"
api PUT "/_ip/graphs/incident?tenant=bench" "$(cat "$here/graph.json")" | head -c 400
api GET "/_ip/graphs/incident?tenant=bench" | head -c 400

run() {
	local label="$1" report="$2"
	curl -s -N -b "$jar" -c "$jar" -X POST -H 'content-type: application/json' -H 'x-ip-csrf: 1' \
		-w '\n[status %{http_code}]\n' \
		-d "{\"input\":$(printf '%s' "$report" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')}" \
		"$host/_ip/graph_run/incident?tenant=bench" >"$out/$label.sse"
	python3 "$here/report.py" "$out/$label.sse" --answers "$out/$label"
}

say "the run on an incident that is still going on (a real model answers twice; this takes minutes)"
run escalated "$ongoing"

say "the run on one that is over and understood — the same graph, a second way out"
run closed "$settled"

say "the run on one that is going on and affects nobody — the same graph, the third way out"
run shelved "$quiet"

cp "$work/server.log" "$out/server.log" 2>/dev/null
echo
echo "the streams: $out/{escalated,closed,shelved}.sse"
