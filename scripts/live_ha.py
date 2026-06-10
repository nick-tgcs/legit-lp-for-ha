#!/usr/bin/env python3
"""Authorized live-HA API helper (deploy/probe tooling).

Auth pattern matches the repo's capture.py / pull_ha_dashboard.sh: read a
refresh token from /config/.storage/auth over SSH, exchange it for a
short-lived access token. Nothing is persisted.

Usage:
  live_ha.py get  /api/hassio/info
  live_ha.py post /api/hassio/store/repositories '{"repository": "https://github.com/nick-tgcs/legit-lp-for-ha"}'
"""

import json
import subprocess
import sys
import urllib.parse
import urllib.request

REMOTE = "root@ha.ngura.agren.au"
API = "http://192.168.51.10:8123"


def token() -> str:
    auth = json.loads(
        subprocess.run(
            ["ssh", REMOTE, "cat /config/.storage/auth"],
            capture_output=True, text=True, check=True,
        ).stdout
    )
    # /api/hassio/* (Supervisor proxy) requires an ADMIN user's token.
    admins = {
        u["id"] for u in auth["data"]["users"]
        if "system-admin" in u.get("group_ids", []) and u.get("is_active")
    }
    rt = next(
        t for t in auth["data"]["refresh_tokens"]
        if t.get("token_type") == "normal" and t.get("client_id")
        and t.get("user_id") in admins
    )
    data = urllib.parse.urlencode({
        "grant_type": "refresh_token",
        "refresh_token": rt["token"],
        "client_id": rt["client_id"],
    }).encode()
    with urllib.request.urlopen(f"{API}/auth/token", data=data) as r:
        return json.load(r)["access_token"]


def main() -> int:
    method, path = sys.argv[1].upper(), sys.argv[2]
    body = sys.argv[3].encode() if len(sys.argv) > 3 else None
    req = urllib.request.Request(
        f"{API}{path}",
        data=body,
        method=method,
        headers={
            "Authorization": f"Bearer {token()}",
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=300) as r:
            print(r.read().decode())
    except urllib.error.HTTPError as e:
        print(f"HTTP {e.code}: {e.read().decode()}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
