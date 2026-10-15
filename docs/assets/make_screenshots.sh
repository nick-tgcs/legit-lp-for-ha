#!/usr/bin/env bash
# Regenerate the README panel screenshots from the real frontend + demo report.
# Requires: python3, playwright CLI with a Chrome/Chromium channel.
#   ./make_screenshots.sh
set -euo pipefail
cd "$(dirname "$0")"
PORT=8765

python3 gen.py

python3 serve.py "$PORT" &
SRV=$!
trap 'kill "$SRV" 2>/dev/null || true' EXIT
# wait for the server
for _ in $(seq 1 50); do
  curl -fsS "http://127.0.0.1:$PORT/api/status" >/dev/null 2>&1 && break
  sleep 0.1
done

shoot() { # scheme file [extra args...]
  local scheme="$1" file="$2"; shift 2
  playwright screenshot --channel chrome --color-scheme "$scheme" \
    --wait-for-selector "#loads .card" "$@" \
    "http://127.0.0.1:$PORT/" "$file"
  echo "wrote $file"
}

shoot light panel-light.png --full-page
shoot dark  panel-dark.png  --full-page
# a narrow mobile view to show the responsive load grid stacking
# (Pixel 7 is a Chromium-based descriptor, so it works with --channel chrome)
shoot light panel-mobile.png --device "Pixel 7" --full-page
