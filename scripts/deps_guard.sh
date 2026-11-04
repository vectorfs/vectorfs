#!/usr/bin/env bash
# deps_guard.sh — machine-enforced dependency red lines (design.md §5.7, §8):
#
#   1. ZERO AWS SDK: any `aws-*` crate in the product dependency graph fails,
#      EXCEPT `aws-lc-rs` / `aws-lc-sys` (the crypto provider hard-bound to
#      object_store's "aws" feature — a C library, NOT part of the SDK chain;
#      accepted in design.md §8 risk #11). The SigV4 verify is hand-rolled
#      (vf/src/sigv4.rs) and the S3 client is object_store's own — the SDK
#      chain (aws-config -> aws-sdk-sso/sts/ssooidc, aws-smithy-*, opendal)
#      must never re-enter, e.g. via `lance`/`lance-io` default features.
#
#   2. Arrow version unification: two different versions of the same
#      `arrow-*` (or `lance-*`) crate fail the build (design.md §8 risk #8,
#      "双 arrow 风险" — API must flow through exactly one arrow generation).
#
# Prints a summary for CI logs. Exit 0 = clean, 1 = violation.
set -euo pipefail
cd "$(dirname "$0")/.."

# --workspace covers all members; spike/ is excluded from the workspace and
# thus not checked (throwaway crate, not product). Lines are normalized to
# `name vX.Y.Z` by taking the first two whitespace fields — this survives both
# the `(*)` repeat marker and ANSI color codes (CI sets CARGO_TERM_COLOR=always,
# which broke the old `sed 's/ (.*)$//'` strip and false-flagged same-version
# multi-path entries as version conflicts). `--color never` belt-and-braces.
list=$(cargo tree --workspace --prefix none --color never \
    | awk 'NF>=2 && $2 ~ /^v[0-9]/ {print $1, $2}' | sort -u)
total=$(grep -c ' v' <<<"$list" || true)

echo "== deps_guard summary =="
echo "distinct crate entries (cargo tree --workspace): ${total}"

# ---- 1. zero-AWS-SDK red line -----------------------------------------------
aws_all=$(grep -E '^aws-[a-z0-9-]+ v' <<<"$list" || true)
aws_sdt=
if [[ -n "$aws_all" ]]; then
    # carve out the accepted crypto provider (aws-lc-rs / aws-lc-sys)
    aws_sdt=$(grep -vE '^aws-lc-(rs|sys) v' <<<"$aws_all" || true)
fi
echo "aws-lc crypto provider present: $(grep -cE '^aws-lc-(rs|sys) v' <<<"$list" || true) crate(s) (accepted, design.md §8 #11)"
echo "aws-* SDK crates (must be 0): $(grep -c . <<<"$aws_sdt" || true)"

# ---- 2. arrow / lance version unification ----------------------------------
dup_arrow=$(grep -E '^(arrow|lance|lancedb)-?[a-z0-9-]* v' <<<"$list" \
    | awk '{print $1}' | sort | uniq -d || true)
echo "arrow-*/lance-*/lancedb crates with >1 version (must be empty): '${dup_arrow//$'\n'/, }'"

fail=0
if [[ -n "$aws_sdt" ]]; then
    echo "FAIL: aws-* SDK crates found in dependency graph:" >&2
    grep . <<<"$aws_sdt" >&2
    fail=1
fi
if [[ -n "$dup_arrow" ]]; then
    echo "FAIL: multiple versions of arrow/lance crates found:" >&2
    while read -r c; do
        grep -E "^${c} v" <<<"$list" >&2
    done <<<"$dup_arrow"
    fail=1
fi

if [[ "$fail" -ne 0 ]]; then
    exit 1
fi
echo "deps_guard: PASS (0 aws-* SDK crates, unified arrow/lance versions)"
