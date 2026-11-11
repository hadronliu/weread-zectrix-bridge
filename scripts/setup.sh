#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in node npm cargo cmake; do
  command -v "$tool" >/dev/null || { echo "Missing dependency: $tool" >&2; exit 1; }
done

# 浏览器选择：默认 auto（系统 Chrome 优先，免下载 Chromium）。
# 需要强制使用 Playwright Chromium 时：WEREAD_BROWSER=chromium bash scripts/setup.sh
BROWSER_MODE="${WEREAD_BROWSER:-auto}"
if [[ "$BROWSER_MODE" == "chromium" ]]; then
  echo "Browser mode: chromium（将下载 Playwright Chromium）"
elif [[ -d "/Applications/Google Chrome.app" ]]; then
  echo "Browser mode: $BROWSER_MODE（检测到系统 Google Chrome，无需下载 Chromium）"
else
  echo "Browser mode: $BROWSER_MODE（未检测到系统 Chrome，将下载 Playwright Chromium）"
fi

(cd browser-controller && npm ci)

# 仅在需要 Chromium 时下载；系统已有 Chrome 且非 chromium 模式时跳过（约省 120MB）。
if [[ "$BROWSER_MODE" == "chromium" ]] || [[ ! -d "/Applications/Google Chrome.app" ]]; then
  (cd browser-controller && npx playwright install chromium)
fi

CMAKE_POLICY_VERSION_MINIMUM=3.5 cargo build --release --locked
echo 'Setup complete. Copy .env.example to .env（ZECTRIX_IP 现在可选）.'
