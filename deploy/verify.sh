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

# --- the proxy's address, derived rather than declared ------------------------
#
# It has to come from the same variable the subnet does. When the subnet was
# hard-coded and the address was independent, a second stack could end up with an
# address outside its own network — and worse, `ADJUTANT_TRUSTED_PROXIES` is a
# literal comparison, so the failure mode is a proxy that is no longer recognised
# and a rate limiter that silently keys everyone to one bucket.
#
# PROXY_IP may still be set for compatibility, and if it is it has to agree. A
# stale value is more dangerous than none, because it looks like configuration.
PREFIX="${ADJUTANT_EDGE_PREFIX:-172.31.7}"
DERIVED_PROXY_IP="${PREFIX}.2"
if [ -n "${PROXY_IP:-}" ] && [ "$PROXY_IP" != "$DERIVED_PROXY_IP" ]; then
  fail "PROXY_IP=$PROXY_IP disagrees with ADJUTANT_EDGE_PREFIX=$PREFIX, which puts the proxy at $DERIVED_PROXY_IP. Set one, not both."
fi
PROXY_IP="$DERIVED_PROXY_IP"
export PROXY_IP

# This stack's compose project name, so the precondition checks can tell our own
# containers and networks from a foreign stack's. Dummy secrets because
# `compose config` will not render without the required variables, and this only
# reads the name.
PROJECT="$(POSTGRES_PASSWORD=x ADJUTANT_APP_PASSWORD=x DOMAIN=x ACME_EMAIL=x \
  docker compose -f "$HERE/compose.proxy.yml" config --format json 2>/dev/null | python3 -c '
import json, sys
try:
    print(json.load(sys.stdin).get("name", ""))
except Exception:
    print("")
')"
[ -n "$PROJECT" ] || fail "cannot determine this stack compose project name; is compose.proxy.yml readable?"

echo "== precondition — the edge subnet is free =="
# Docker refuses a second network on an overlapping pool, and its error —
# "Pool overlaps with other one on this address space" — names neither the subnet
# nor the network already holding it. That is the least legible failure in this
# directory, so ask first and say which.
EDGE_SUBNET="${PREFIX}.0/24"
collision="$(docker network ls --format '{{.Name}}' | python3 -c '
import ipaddress, subprocess, sys
want = ipaddress.ip_network(sys.argv[1])
mine = sys.argv[2]
hits = []
for name in sys.stdin.read().split():
    info = subprocess.run(
        ["docker", "network", "inspect", name, "--format",
         "{{index .Labels \"com.docker.compose.project\"}}|{{range .IPAM.Config}}{{.Subnet}} {{end}}"],
        capture_output=True, text=True).stdout.strip()
    project, _, subnets = info.partition("|")
    # This stack own network is meant to be on this subnet — it is the one about
    # to be reused. Excluding it is not a convenience: without the exclusion the
    # check fails on a correct stack. (The first attempt excluded on
    # `com.docker.compose.project.working_dir`, which networks do not carry at
    # all, so it silently never matched. `project` is the label they do have.)
    if project == mine:
        continue
    for s in subnets.split():
        try:
            if ipaddress.ip_network(s).overlaps(want):
                hits.append(name + " (" + s + ")")
        except ValueError:
            pass
print(", ".join(hits))
' "$EDGE_SUBNET" "$PROJECT")"
if [ -n "$collision" ]; then
  fail "the edge subnet $EDGE_SUBNET overlaps a network that already exists: $collision
       Two Adjutant stacks on one host need different subnets. Set
       ADJUTANT_EDGE_PREFIX to a different third octet (for example 172.31.8) and
       bring this stack up again."
fi
ok "no existing network overlaps $EDGE_SUBNET"

echo "== precondition — the edge ports are free =="
# Distinct subnets are not enough for a second stack: two of them cannot both
# publish 80 and 443, and the daemon's complaint about that arrives *after* the
# networks, volumes and database have been created. Ask first, and name what is
# holding the port.
#
# This stack's OWN proxy is excluded, and that exclusion is the whole point: the
# first version of this check counted it, which made `verify.sh` fail on the very
# deployment it exists to prove. A precondition that cannot pass on a correct
# stack is worse than no precondition.
busy="$(docker ps --format '{{.Names}}\t{{.Label "com.docker.compose.project"}}\t{{.Ports}}' | python3 -c '
import sys
mine = sys.argv[1]
hits = []
for line in sys.stdin:
    parts = line.rstrip("\n").split("\t")
    if len(parts) != 3:
        continue
    name, project, ports = parts
    if project == mine:
        continue          # ours, and it is supposed to be holding them
    for port in ("80", "443"):
        if ("0.0.0.0:" + port + "->") in ports or ("[::]:" + port + "->") in ports:
            hits.append(name + " holds " + port)
            break
print("; ".join(hits))
' "$PROJECT")"
if [ -n "$busy" ]; then
  fail "port 80 or 443 is already published: $busy
       A second Adjutant stack on this host needs different published ports, or
       it is not a second stack — one stack serves a name on 80 and 443."
fi
ok "nothing outside this stack is publishing 80 or 443"
docker compose version >/dev/null 2>&1 || fail "docker compose v2 is not available"

# One request from a fresh container, printing just the status code.
code() { # $1 = shell command, $2 = probe service (probe, or probe2 for a second client)
  "${COMPOSE[@]}" run --rm --no-deps "${2:-probe}" "$1" 2>/dev/null | tr -d '\r'
}

# The limiter's window, in seconds. Deliberately short: the readiness probe below
# spends the *first* client's budget, and the only way the proofs can then observe
# a 200 is if that bucket rolls over first. A 60s window would mean a 60s sleep
# between proofs; 5s is enough to be unambiguous and keeps the whole run quick.
WINDOW=5
# Wait past the window so the address named (default: the first probe) starts clean.
fresh() { sleep $((WINDOW + 1)); }

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
# Read this from the *rendered* configuration, not from `docker compose port`.
# On Compose v5.5.1 `port` prints the literal string `invalid IP:0` and exits 0
# for a service that publishes nothing, so a `[ -z ]` test on its output fails a
# perfectly correct stack — which is exactly what this proof did on its first
# real run. `config` is the authoritative render, and the drift check above
# already depends on it.
published="$("${COMPOSE[@]}" config --format json 2>/dev/null | python3 -c '
import json, sys
svc = json.load(sys.stdin)["services"]["adjutant"]
print(",".join("%s:%s" % (p.get("published", ""), p.get("target", ""))
               for p in (svc.get("ports") or [])))
')"
[ -z "$published" ] || fail "adjutant publishes $published: anything on the network can skip TLS and the proxy"
ok "no host port is published for adjutant"

echo "== the stack, with the limiter observable (1 request per 60s per client) =="
ADJUTANT_RATE_MAX=1 ADJUTANT_RATE_WINDOW=$WINDOW "${COMPOSE[@]}" up -d postgres adjutant caddy >/dev/null

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

fresh
echo "== proof 2 — the real client is the key, not the proxy =="
# Client A: two requests, one container, therefore one bucket. Expect 200 then 429.
a="$(code "curl -s -o /dev/null -w '%{http_code}\n' $THROUGH; curl -s -o /dev/null -w '%{http_code}\n' $THROUGH")"
[ "$a" = "$(printf '200\n429')" ] || fail "client A got [$a], expected 200 then 429 — either limiting is off or the peer is not being keyed per client"
ok "client A's own budget is one request (200 then 429)"

# Client B: a second container, so a second address. Its first request must be 200
# — if the app ignored the forwarded header, every request through the proxy would
# key on the proxy's address and B would inherit A's exhausted budget.
b="$(code "curl -s -o /dev/null -w '%{http_code}' $THROUGH" probe2)"
[ "$b" = "200" ] || fail "client B got $b, expected 200 — the forwarded client address is not being used, so every client behind the proxy shares one bucket"
ok "client B has its own budget: the forwarded address is the key"

fresh
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
