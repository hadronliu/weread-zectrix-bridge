#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in node npm cargo cmake; do
  command -v "$tool" >/dev/null || { echo "Missing dependency: $tool" >&2; exit 1; }
done
(cd browser-controller && npm ci && npx playwright install chromium)
CMAKE_POLICY_VERSION_MINIMUM=3.5 cargo build --release --locked
echo 'Setup complete. Copy .env.example to .env and set ZECTRIX_IP.'
