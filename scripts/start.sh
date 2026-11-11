#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ -f .env ]]; then set -a; source .env; set +a; fi
[[ -x target/release/weread-zectrix-bridge ]] || { echo 'Run scripts/setup.sh first' >&2; exit 1; }
node_path="$(command -v node)"
args=(--node-bin "$node_path" --controller "$PWD/browser-controller/controller.js")
# 设备 IP 可省略：省略时服务照常启动，在 .env 补充后可经管理页或重启连接。
if [[ -n "${ZECTRIX_IP:-}" ]]; then args+=(--ip "$ZECTRIX_IP"); fi
args+=(--format "${ZECTRIX_FORMAT:-gray4}" --browser "${WEREAD_BROWSER:-auto}")
if [[ -n "${WEREAD_HTTP_PORT:-}" ]]; then args+=(--http-port "$WEREAD_HTTP_PORT"); fi
if [[ -n "${WEREAD_INK_BLACK_POINT:-}" ]]; then args+=(--black-point "$WEREAD_INK_BLACK_POINT"); fi
exec ./target/release/weread-zectrix-bridge "${args[@]}" "$@"
