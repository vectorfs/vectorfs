#!/usr/bin/env bash
# scripts/cli_smoke.sh — M2 T6 bash CLI smoke (appendix E.4 full chain):
#   init -> serve -> put (foreground wait) -> get (byte compare) -> search
#   -> sources list -> rm -> reindex -> status
# Any failure exits non-zero (set -e + explicit checks). Hermetic inputs: the
# committed tiny-embedder fixture and the repo-local ONNX Runtime .so
# (VF_ORT_PATH); no network beyond loopback.
#
# Reused by T9 / the Docker image smoke: override binary locations with
# VF_BIN_DIR (defaults to a cargo build of this checkout) and the port with
# VF_PORT.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CARGO_BIN="${CARGO:-cargo}"
BIN_DIR="${VF_BIN_DIR:-$REPO_ROOT/target/debug}"
VECTORFS="$BIN_DIR/vectorfs"
VF="$BIN_DIR/vf"
PORT="${VF_PORT:-8791}"
FIXTURE="$REPO_ROOT/vf/tests/fixtures/tiny-embedder"
ORT_LIB="${VF_ORT_PATH:-$REPO_ROOT/runtime-libs/libonnxruntime.so}"

fail() { echo "cli_smoke: FAIL: $*" >&2; exit 1; }

[[ -f "$FIXTURE/model.onnx" ]] || fail "tiny-embedder fixture missing at $FIXTURE"
[[ -f "$ORT_LIB" ]] || fail "ONNX Runtime missing at $ORT_LIB (scripts/fetch_runtime_libs.sh onnxruntime)"

# binaries: use what is there, build when absent
if [[ ! -x "$VECTORFS" || ! -x "$VF" ]]; then
    echo "cli_smoke: building debug binaries ($BIN_DIR)…"
    (cd "$REPO_ROOT" && "$CARGO_BIN" build --bins)
fi
[[ -x "$VECTORFS" ]] || fail "vectorfs binary not found/executable at $VECTORFS"
[[ -x "$VF" ]] || fail "vf binary not found/executable at $VF"

WORK="$(mktemp -d /tmp/vf-cli-smoke.XXXXXX)"
DATA="$WORK/data"
SERVE_PID=""
cleanup() {
    [[ -n "$SERVE_PID" ]] && kill "$SERVE_PID" 2>/dev/null || true
    [[ -n "$SERVE_PID" ]] && wait "$SERVE_PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

echo "== init =="
"$VECTORFS" init --data-dir "$DATA" >/dev/null
TOML="$DATA/.vectorfs/vectorfs.toml"
[[ -f "$TOML" ]] || fail "init did not write $TOML"

# credentials: printed by init AND stored in the toml — read them back out of
# the file (the client is a separate process by design, it never reads the
# daemon's toml itself)
AK="$(grep -E '^access_key_id' "$TOML" | head -1 | cut -d'"' -f2)"
SK="$(grep -E '^secret_access_key' "$TOML" | head -1 | cut -d'"' -f2)"
[[ -n "$AK" && -n "$SK" ]] || fail "cannot parse credentials from $TOML"

# append the file-layer pipeline (tiny fixture, txt -> docs/** scope)
cat >> "$TOML" <<EOF

[extractor.plain]
kind = "txt"

[embedder.tiny]
kind = "ort"
model = "tiny-fixture"
path = "$FIXTURE"
offline = true

[ingest.docs]
scope = "docs/**"
extract = { txt = "plain" }
chunk = { kind = "token-window", size = 32, overlap = 8 }
embedders = ["tiny"]
scan_interval_secs = 1
EOF

echo "== serve (port $PORT) =="
VF_ORT_PATH="$ORT_LIB" "$VECTORFS" serve --data-dir "$DATA" --port "$PORT" \
    >"$WORK/serve.log" 2>&1 &
SERVE_PID=$!

# wait for the no-auth /health probe (up to 60s; the ingest worker + models
# load before the listener binds)
healthy=""
for _ in $(seq 1 300); do
    if exec 2>/dev/null 3<>"/dev/tcp/127.0.0.1/$PORT"; then
        printf 'GET /health HTTP/1.1\r\nhost: smoke\r\nconnection: close\r\n\r\n' >&3
        resp="$(head -c 200 <&3 || true)"
        exec 3<&- 2>/dev/null || true
        [[ "$resp" == *'"ok":true'* ]] && { healthy=1; break; }
    fi
    sleep 0.2
done
[[ -n "$healthy" ]] || { tail -30 "$WORK/serve.log" >&2; fail "serve never became healthy"; }
echo "serve healthy"

export VF_ENDPOINT="http://127.0.0.1:$PORT" VF_ACCESS_KEY="$AK" VF_SECRET_KEY="$SK"

echo "== put (foreground: waits for INDEXED) =="
printf 'the quick brown fox jumps over the lazy dog' > "$WORK/fox.txt"
put_out="$("$VF" put "$WORK/fox.txt" --bucket docs --key docs/fox.txt --timeout 120)" \
    || { echo "$WORK/serve.log tail:" >&2; tail -30 "$WORK/serve.log" >&2; fail "vf put"; }
echo "put -> $put_out"
[[ "$put_out" == "INDEXED"$'\t'"docs/fox.txt" ]] || fail "put did not reach INDEXED: $put_out"

echo "== get (byte round trip) =="
"$VF" get docs/fox.txt --bucket docs --out "$WORK/roundtrip.txt"
cmp -s "$WORK/fox.txt" "$WORK/roundtrip.txt" || fail "get bytes differ from the upload"

echo "== search =="
search_out="$("$VF" search "quick brown fox" --bucket docs --top-k 3)"
echo "$search_out"
grep -q "docs/fox.txt" <<<"$search_out" || fail "search hits do not reference the source"

echo "== sources list =="
list_out="$("$VF" sources list --bucket docs)"
echo "$list_out"
grep -q "docs/fox.txt" <<<"$list_out" || fail "sources list missing the file"

echo "== rm =="
rm_out="$("$VF" rm docs/fox.txt --bucket docs)"
echo "rm -> $rm_out"
[[ "$rm_out" == "DELETING"$'\t'"docs/fox.txt" ]] || fail "rm did not answer DELETING: $rm_out"

echo "== reindex =="
job="$("$VF" reindex --bucket docs)"
echo "reindex jobId -> $job"
[[ "${#job}" -eq 32 && "$job" =~ ^[0-9a-f]{32}$ ]] || fail "reindex did not print a 32-hex jobId: $job"

echo "== status =="
status_out="$("$VF" status)"
echo "$status_out"
grep -q "bucket docs" <<<"$status_out" || fail "status missing the ingest bucket"

echo
echo "cli_smoke: PASS (all verbs, hermetic fixture, port $PORT)"
