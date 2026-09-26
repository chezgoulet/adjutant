#!/usr/bin/env bash
# The three proofs of deploy/README.md, run against a real host.
#
#   ./deploy/verify.sh
#
# It fails loudly and never skips: a proof that cannot run is not a pass. Needs
# Docker with Compose v2, and the stack already bootstrapped (see the README).
#
# Why the rate limiter is turned down to 1: it makes the limiter's *choice of key*
# observable as a 200/429 without reading any log. That is the whole trick — with
# the default limit, a wrong key and a right key look identical.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
COMPOSE=(docker compose -f "$HERE/compose.proxy.yml")
THROUGH=http://caddy:8080/api/hello      # through the proxy, on the origin listener
DIRECT=http://adjutant:8787/api/hello    # straight at the app, a peer the app does not trust

fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok — $*"; }

command -v docker >/dev/null 2>&1 || fail "docker is not installed; this proof needs a host with Docker and Compose v2"
docker compose version >/dev/null 2>&1 || fail "docker compose v2 is not available"
[ -n "${PROXY_IP:-}" ] || fail "set PROXY_IP (the proxy's static address, as in deploy/compose.proxy.yml)"

# One request from a fresh container, printing just the status code.
code() { # $1 = shell command for the probe
  "${COMPOSE[@]}" run --rm --no-deps probe "$1" 2>/dev/null | tr -d '\r'
}

echo "== check — the two compose files' adjutant service has not drifted =="
# deploy/compose.proxy.yml duplicates the base stack on purpose (Compose
# concatenates `ports`, so an override cannot un-publish the app's port). That
# duplication is a maintenance hazard, so it is checked rather than trusted.
# Dummy secrets: this compares structure, and `compose config` needs the
# required variables to be present before it will render.
b="$(mktemp)"; f="$(mktemp)"
POSTGRES_PASSWORD=x ADJUTANT_APP_PASSWORD=x PROXY_IP="${PROXY_IP}" DOMAIN=x ACME_EMAIL=x \
  docker compose -f "$ROOT/docker-compose.yml" config --format json >"$b" 2>/dev/null \
  || fail "could not render docker-compose.yml (docker compose config)"
POSTGRES_PASSWORD=x ADJUTANT_APP_PASSWORD=x PROXY_IP="${PROXY_IP}" DOMAIN=x ACME_EMAIL=x \
  docker compose -f "$HERE/compose.proxy.yml" config --format json >"$f" 2>/dev/null \
  || fail "could not render deploy/compose.proxy.yml (docker compose config)"
python3 - "$b" "$f" <<'PY' || { rm -f "$b" "$f"; exit 1; }
import json, sys
# Differences that are the point of the file, not drift.
IGNORE_KEYS = {"ports", "networks"}
IGNORE_ENV = {"ADJUTANT_TRUSTED_PROXIES", "ADJUTANT_CORS"}

def service(path):
    svc = json.load(open(path))["services"]["adjutant"]
    for key in IGNORE_KEYS:
        svc.pop(key, None)
    env = svc.get("environment") or {}
    for key in IGNORE_ENV:
        env.pop(key, None)
    svc["environment"] = env
    return svc

base, front = service(sys.argv[1]), service(sys.argv[2])
if base != front:
    keys = sorted(set(base) | set(front))
    differing = [k for k in keys if base.get(k) != front.get(k)]
    print("FAIL: the `adjutant` service differs between docker-compose.yml and deploy/compose.proxy.yml")
    for key in differing:
        print(f"  {key} base : {json.dumps(base.get(key))[:300]}")
        print(f"  {key} front: {json.dumps(front.get(key))[:300]}")
    sys.exit(1)
print("  ok — identical apart from the port, the networks, and the trust/CORS settings")
PY
rm -f "$b" "$f"

echo "== proof 1 — the proxy is the only way in =="
published="$("${COMPOSE[@]}" port adjutant 8787 2>/dev/null || true)"
[ -z "$published" ] || fail "adjutant publishes $published: anything on the network can skip TLS and the proxy"
ok "no host port is published for adjutant"

echo "== the stack, with the limiter observable (1 request per 60s per client) =="
ADJUTANT_RATE_MAX=1 ADJUTANT_RATE_WINDOW=60 "${COMPOSE[@]}" up -d postgres adjutant caddy >/dev/null

ready=0
for _ in $(seq 1 60); do
  if [ "$(code "curl -s -o /dev/null -w '%{http_code}' $THROUGH")" = "200" ]; then ready=1; break; fi
  sleep 2
done
if [ "$ready" != "1" ]; then
  echo "--- adjutant's own output (last 30 lines) ---" >&2
  "${COMPOSE[@]}" logs --tail 30 adjutant >&2 || true
  fail "the app never answered 200 at $THROUGH through the proxy — a dead process is not a network problem"
fi
ok "the app answers through the proxy"

echo "== proof 2 — the real client is the key, not the proxy =="
# Client A: two requests, one container, therefore one bucket. Expect 200 then 429.
a="$(code "curl -s -o /dev/null -w '%{http_code}\n' $THROUGH; curl -s -o /dev/null -w '%{http_code}\n' $THROUGH")"
[ "$a" = "$(printf '200\n429')" ] || fail "client A got [$a], expected 200 then 429 — either limiting is off or the peer is not being keyed per client"
ok "client A's own budget is one request (200 then 429)"

# Client B: a second container, so a second address. Its first request must be 200
# — if the app ignored the forwarded header, every request through the proxy would
# key on the proxy's address and B would inherit A's exhausted budget.
b="$(code "curl -s -o /dev/null -w '%{http_code}' $THROUGH")"
[ "$b" = "200" ] || fail "client B got $b, expected 200 — the forwarded client address is not being used, so every client behind the proxy shares one bucket"
ok "client B has its own budget: the forwarded address is the key"

echo "== proof 3 — a forged header from an untrusted peer buys nothing =="
# One container, two requests, a different forged header each time. The key must
# stay the peer, so the second is refused. This is the attack the setting exists
# to stop: rotating a header to reset your own budget.
forged="$(code "curl -s -o /dev/null -w '%{http_code}\n' -H 'x-forwarded-for: 203.0.113.7' $DIRECT; curl -s -o /dev/null -w '%{http_code}\n' -H 'x-forwarded-for: 203.0.113.8' $DIRECT")"
[ "$forged" = "$(printf '200\n429')" ] || fail "a forged x-forwarded-for from an untrusted peer got [$forged], expected 200 then 429 — the header is being believed from a peer that is not the proxy"
ok "rotating a forged header did not produce a second bucket"

echo
echo "all three proofs passed against PROXY_IP=$PROXY_IP"
echo
echo "Record this transcript in the PR and a line in docs/release-path.md Stage 3.1,"
echo "then choose the deployment host — that is what Stage 3.1 was waiting for."
