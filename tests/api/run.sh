#!/usr/bin/env bash
# Runs the api suite against a server of its own: a scratch sqlite and sled, one system
# administrator made the way an operator makes one, and every `.hurl` file beside this one.
#
#   tests/api/run.sh [hurl file ...]
#
# The server, its database and its cache go when the run does. Nothing here touches a
# database anything else is using.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
port="${IP_PORT:-18080}"
host="http://127.0.0.1:$port"
passphrase="correct horse staple"
upstream_port="${IP_UPSTREAM_PORT:-18081}"
work="$(mktemp -d)"
drsg_port="${IP_DRSG_PORT:-18082}"
drsg_token="drsg-$$"
server=""
upstream=""
store=""

cleanup() {
	for pid in "$server" "$upstream" "$store"; do
		if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
			kill "$pid" 2>/dev/null || true
			wait "$pid" 2>/dev/null || true
		fi
	done
	rm -rf "$work"
}
trap cleanup EXIT

cd "$root"
echo "building the standalone server"
cargo build --quiet --no-default-features --features standalone-storage,standalone-cache
cargo run --quiet -p ip-plugin --example emit_plugin -- "$work/plugin.wasm" >/dev/null

cat >"$work/intelli-prism.toml" <<TOML
[server]
listen = "127.0.0.1:$port"

[storage]
backend = "sqlite"
path = "$work/api.db"

[cache]
backend = "sled"
path = "$work/api-cache"

[auth]
nonce_ttl = 300

[auth.jwt]
issuer = "intelli-prism"
secret = "0123456789abcdef0123456789abcdef"
ttl = 3600

# An agent run is the only thing here that reaches an upstream, and it has to have one
# before a turn has anywhere to go.
[[upstream]]
id = "anthropic"
base_url = "http://127.0.0.1:$upstream_port/v1"
api_key = "sk-suite"
model = "claude-opus-5"
caller_speaks = "anthropic"
upstream_speaks = "anthropic"
TOML

# The run graphs live in a dr-strange plane rather than in our own tables, so the files that
# exercise them need one running. Without `drsg` on the path there is nothing to keep a graph
# in, and those files are left out rather than failed: every other endpoint is unaffected.
graphs="no"
if command -v drsg >/dev/null 2>&1; then
	DRSG_TOKEN="$drsg_token" drsg serve --addr "127.0.0.1:$drsg_port" -d "$work/graphs.drsg" \
		>"$work/drsg.log" 2>&1 &
	store=$!
	rpc() {
		curl -s -m 10 -H "Authorization: Bearer $drsg_token" -H 'content-type: application/json' \
			-d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" \
			"http://127.0.0.1:$drsg_port/rpc"
	}
	for _ in $(seq 1 100); do
		rpc plane.list '{}' >/dev/null 2>&1 && break
		kill -0 "$store" 2>/dev/null || break
		sleep 0.1
	done
	if rpc plane.create '{"name":"ip_graphs"}' >/dev/null 2>&1; then
		graphs="yes"
		cat >>"$work/intelli-prism.toml" <<TOML

[graph_store]
url = "http://127.0.0.1:$drsg_port/rpc"
token = "$drsg_token"
plane = "ip_graphs"
TOML
	fi
fi
[ "$graphs" = yes ] || echo "no drsg on the path: the run graph files are left out"

python3 "$here/stub_upstream.py" "$upstream_port" &
upstream=$!

binary="$root/target/debug/intelli-prism"
printf '%s' "$passphrase" | "$binary" --config "$work/intelli-prism.toml" admin create root
"$binary" --config "$work/intelli-prism.toml" >"$work/server.log" 2>&1 &
server=$!

echo "waiting for $host"
for _ in $(seq 1 100); do
	if curl --silent --fail "$host/_ip/healthz" >/dev/null 2>&1; then
		break
	fi
	if ! kill -0 "$server" 2>/dev/null; then
		echo "the server stopped before it listened:" >&2
		cat "$work/server.log" >&2
		exit 1
	fi
	sleep 0.1
done

files=("$@")
if [ ${#files[@]} -eq 0 ]; then
	if [ "$graphs" = yes ]; then
		mapfile -t files < <(find "$here" -name '*.hurl' | sort)
	else
		mapfile -t files < <(find "$here" -name '*.hurl' -not -name '1[56]_graph*' | sort)
	fi
fi

# The files run one after another, against one server, so each builds on what the last left.
# A body read from a file has to sit under the file root, which is where the plugin is written.
hurl --test --jobs 1 \
	--file-root "$work" \
	--variable "host=$host" \
	--variable "passphrase=$passphrase" \
	--variable "plugin=plugin.wasm" \
	"${files[@]}"
