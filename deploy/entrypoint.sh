#!/bin/sh
# vectorfs container entrypoint (M2-T8, design.md §5.10.7).
#
# Contract (POSIX sh, set -eu; the script always `exec`s the final process
# so the daemon itself is PID 1 and receives SIGTERM directly):
#
#   docker run <img>                      first-boot materialize (VF_PRESET)
#                                         then serve the data volume
#   docker run <img> serve …              operator-owned serve flags: exec
#                                         `vectorfs serve …` with NO
#                                         first-boot init (the emulator/test
#                                         shape, e.g. `serve --backend memory
#                                         --no-auth`, owns its lifecycle
#                                         entirely)
#   docker run <img> <verb> …             other vectorfs verbs (init/status/
#                                         doctor/pipelines/mcp/optimize) are
#                                         prefixed with the binary — docker
#                                         command overrides carry SUBCOMMANDS
#   docker run <img> <anything else>      exec verbatim (vf, /bin/sh, …)
#
# First boot: VF_PRESET non-empty AND no <data>/.vectorfs/vectorfs.toml ⇒
# `vectorfs init --preset $VF_PRESET --models-dir /vectorfs/models` — the
# models cache is the /vectorfs/models VOLUME (§5.10.7: models never live in
# an image layer). Already initialized ⇒ skip (restarts stay fast). The
# scalar VF_* env (VF_PORT, VF_BACKEND, VF_ACCESS_KEY/VF_SECRET_KEY, …)
# needs no plumbing here: `serve` resolves flag > VF_* env > vectorfs.toml
# itself (config.rs); the image sets VF_LISTEN=0.0.0.0 / VF_DATA_DIR.
set -eu

DATA_DIR="${VF_DATA_DIR:-/vectorfs/data}"
MODELS_DIR="${VF_MODELS_DIR:-/vectorfs/models}"
CONFIG="${DATA_DIR}/.vectorfs/vectorfs.toml"

# Command override. A leading vectorfs VERB is prefixed with the server
# binary (the verb namespace belongs to vectorfs in this image); anything
# else is exec'd verbatim.
case "${1:-}" in
    serve|init|status|doctor|mcp|optimize|pipelines)
        exec vectorfs "$@"
        ;;
esac
if [ "$#" -gt 0 ]; then
    exec "$@"
fi

# Default command: materialize on first boot, then serve.
if [ -n "${VF_PRESET:-}" ]; then
    if [ ! -f "$CONFIG" ]; then
        echo "vectorfs: first boot: init --preset ${VF_PRESET} --data-dir ${DATA_DIR} --models-dir ${MODELS_DIR}" >&2
        vectorfs init --preset "$VF_PRESET" --data-dir "$DATA_DIR" --models-dir "$MODELS_DIR"
    else
        echo "vectorfs: ${CONFIG} exists — skipping first-boot init (VF_PRESET=${VF_PRESET} ignored)" >&2
    fi
elif [ ! -f "$CONFIG" ]; then
    echo "vectorfs: no ${CONFIG} and no VF_PRESET — serving the pure protocol face (set VF_PRESET=dev|text-rag|air-gapped to materialize one)" >&2
fi

exec vectorfs serve --data-dir "$DATA_DIR"
