#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ -f .env ]]; then set -a; source .env; set +a; fi
: "${ZECTRIX_IP:?Set ZECTRIX_IP in .env first}"
[[ -x target/release/weread-zectrix-bridge ]] || { echo 'Run scripts/setup.sh first' >&2; exit 1; }
node_path="$(command -v node)"
args=(--ip "$ZECTRIX_IP" --format "${ZECTRIX_FORMAT:-mono}" --node-bin "$node_path" --controller "$PWD/browser-controller/controller.js")
if [[ "${ZECTRIX_PREFETCH:-0}" != 1 ]]; then args+=(--no-prefetch); fi
exec ./target/release/weread-zectrix-bridge "${args[@]}" "$@"
