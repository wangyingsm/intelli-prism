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
work="$(mktemp -d)"
server=""

cleanup() {
	if [ -n "$server" ] && kill -0 "$server" 2>/dev/null; then
		kill "$server" 2>/dev/null || true
		wait "$server" 2>/dev/null || true
	fi
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
TOML

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
	mapfile -t files < <(find "$here" -name '*.hurl' | sort)
fi

# The files run one after another, against one server, so each builds on what the last left.
# A body read from a file has to sit under the file root, which is where the plugin is written.
hurl --test --jobs 1 \
	--file-root "$work" \
	--variable "host=$host" \
	--variable "passphrase=$passphrase" \
	--variable "plugin=plugin.wasm" \
	"${files[@]}"
