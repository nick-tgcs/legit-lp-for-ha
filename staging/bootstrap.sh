#!/usr/bin/env bash
# Headless onboarding of the staging HA + long-lived token mint -> .token.
# Idempotent: a working .token short-circuits; a stored .refresh re-mints.
set -euo pipefail
cd "$(dirname "$0")"

HA_URL="${HA_URL:-http://localhost:8123}"
CLIENT_ID="$HA_URL/"
COMPOSE="docker compose"

ok() { curl -fsS -m 5 -H "Authorization: Bearer $1" "$HA_URL/api/config" >/dev/null 2>&1; }

if [[ -f .token ]] && ok "$(cat .token)"; then
  echo "bootstrap: existing .token works"
  exit 0
fi

echo "bootstrap: waiting for HA at $HA_URL ..."
for _ in $(seq 1 120); do
  curl -fsS -m 2 "$HA_URL/api/onboarding" >/dev/null 2>&1 && break
  sleep 2
done
curl -fsS -m 2 "$HA_URL/api/onboarding" >/dev/null

# Onboard (create owner) if not already done.
if curl -fsS "$HA_URL/api/onboarding" | grep -q '"user".*false' 2>/dev/null || \
   curl -fsS "$HA_URL/api/onboarding" | python3 -c 'import json,sys; sys.exit(0 if any(s["step"]=="user" and not s["done"] for s in json.load(sys.stdin)) else 1)'; then
  echo "bootstrap: onboarding owner user"
  AUTH_CODE=$(curl -fsS -X POST "$HA_URL/api/onboarding/users" \
    -H 'Content-Type: application/json' \
    -d "{\"client_id\": \"$CLIENT_ID\", \"name\": \"Staging\", \"username\": \"staging\", \"password\": \"staging-Pass1\", \"language\": \"en\"}" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["auth_code"])')
  TOKENS=$(curl -fsS -X POST "$HA_URL/auth/token" \
    -d "grant_type=authorization_code&code=$AUTH_CODE&client_id=$CLIENT_ID")
  ACCESS=$(echo "$TOKENS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["access_token"])')
  echo "$TOKENS" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["refresh_token"])' > .refresh
  # Finish the remaining onboarding steps (idempotent-ish; ignore failures).
  curl -fsS -X POST "$HA_URL/api/onboarding/core_config" -H "Authorization: Bearer $ACCESS" -d '{}' >/dev/null || true
  curl -fsS -X POST "$HA_URL/api/onboarding/analytics" -H "Authorization: Bearer $ACCESS" -d '{}' >/dev/null || true
  curl -fsS -X POST "$HA_URL/api/onboarding/integration" -H "Authorization: Bearer $ACCESS" \
    -H 'Content-Type: application/json' \
    -d "{\"client_id\": \"$CLIENT_ID\", \"redirect_uri\": \"${CLIENT_ID}?auth_callback=1\"}" >/dev/null || true
elif [[ -f .refresh ]]; then
  echo "bootstrap: re-minting access token from stored refresh token"
  ACCESS=$(curl -fsS -X POST "$HA_URL/auth/token" \
    -d "grant_type=refresh_token&refresh_token=$(cat .refresh)&client_id=$CLIENT_ID" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["access_token"])')
else
  echo "bootstrap: HA already onboarded but no .refresh available — wipe the runtime config and retry" >&2
  exit 1
fi

# Long-lived token via the WebSocket API, using the HA container's own python
# (aiohttp ships with HA; nothing to install on the host).
echo "bootstrap: minting long-lived access token"
$COMPOSE exec -T -e ACCESS="$ACCESS" homeassistant python3 - > .token <<'PY'
import asyncio, json, os
import aiohttp

async def main():
    async with aiohttp.ClientSession() as s:
        async with s.ws_connect("ws://localhost:8123/api/websocket") as ws:
            await ws.receive_json()  # auth_required
            await ws.send_json({"type": "auth", "access_token": os.environ["ACCESS"]})
            msg = await ws.receive_json()
            assert msg["type"] == "auth_ok", msg
            await ws.send_json({"id": 1, "type": "auth/long_lived_access_token",
                                "client_name": "staging-verify", "lifespan": 365})
            msg = await ws.receive_json()
            assert msg.get("success"), msg
            print(msg["result"])

asyncio.run(main())
PY

ok "$(cat .token)" && echo "bootstrap: .token minted and verified"
