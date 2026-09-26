# Adjutant — the proxy-fronted deployment

This directory is the deployment the M7 box actually asks for: **TLS terminated at
a reverse proxy**, with Adjutant behind it, and the two facts that matter proven
rather than asserted.

The application terminates no TLS and never will (owner decision, 2026-09-26). It
binds a plain HTTP port and reads one header — `x-forwarded-for` — and only from a
peer it has been told to trust. Everything else about certificates, redirects and
HSTS belongs to the proxy.

## What is here

| File | What it is |
|---|---|
| `compose.proxy.yml` | A complete stack — Postgres, Adjutant, and Caddy — on a private network with **static addresses**, which is what makes the trust setting deterministic. Not an override: Compose *concatenates* `ports`, so an override cannot un-publish the app's port, and an app reachable directly is an app reachable without TLS. |
| `Caddyfile` | The default proxy: automatic certificates, HSTS, the plain-HTTP redirect. |
| `traefik/adjutant.yml` | The same shape for Traefik, as a file-provider router — the pattern The House already runs. |
| `verify.sh` | Runs the proofs below against whatever host you point it at, plus a check that the two compose files have not drifted apart. |

## The one setting that matters

`ADJUTANT_TRUSTED_PROXIES` names the peers whose `x-forwarded-for` may be
believed. It is a **literal string comparison** against the address the server
sees as its direct peer, and the default is **empty — the header is ignored
entirely**. That default is deliberate: honouring the header from anyone lets any
client reset its own rate-limit budget by rotating one header value.

Two consequences, both load-bearing:

- **Pin the proxy's address.** On a Compose network the proxy's IP is normally
  dynamic, so a hardcoded trust value would silently stop matching and the app
  would fall back to keying every request on the proxy — one shared bucket for the
  whole troop. `compose.proxy.yml` therefore gives the proxy a static
  `ipv4_address` on a dedicated subnet and passes that same value in. Change one,
  change the other; `verify.sh` checks they agree.
- **IPv4 dotted-quad, matching the bind.** With `ADJUTANT_BIND=0.0.0.0:8787` the
  peer appears as `172.31.7.2`. Bind the wildcard IPv6 address (`[::]:8787`) and
  a v4 peer can appear as `::ffff:172.31.7.2`, which will never string-match a
  dotted-quad entry: the header is then ignored (fail-safe, not fail-open) and
  every client shares one bucket. Leave the bind as it is.

`x-forwarded-for` is walked **from the right**, skipping trusted hops, so anything
a client injects on the left is never used. Appending is what proxies do, and it
is what makes that safe.

## The proofs

`verify.sh` runs three checks. They are cheap, they need no second machine, and
they are falsifiable — each one fails if the setting is wrong rather than passing
because nothing was tried. All three are run with `ADJUTANT_RATE_MAX=1` and
`ADJUTANT_RATE_WINDOW=60` so that the rate limiter's *choice of key* is directly
observable as a 200/429.

**1. The proxy is the only way in.** No host port is published for `adjutant`.
`docker compose port adjutant 8787` prints nothing, and the only listeners are the
proxy's 80/443. If this fails, anything on the network can reach the app over
plain HTTP and skip both TLS and the proxy.

**2. The real client is the key, not the proxy.** Two throwaway containers on the
same network request through the proxy. The first (client A) makes two requests:
the second is `429`, because A's budget is one. Client B's *first* request is
`200`. That 200 is the proof: if the header were ignored, every request through
the proxy would key on the proxy's address and B would inherit A's exhausted
budget. The 429 on A's second request proves the limiter is on at all, so the 200
cannot be explained by limiting being disabled.

**3. A forged header from an untrusted peer buys nothing.** A container on the
network requests the app **directly** — a peer that is not in
`ADJUTANT_TRUSTED_PROXIES` — twice, with a *different* forged
`x-forwarded-for` each time. The second is `429`. Rotating the header did not
produce a new bucket, which is exactly the attack the setting exists to stop. Run
with the *same* configuration as proof 2, this also shows the trust is per-peer
rather than global.

## Running it

```bash
export DOMAIN=adjutant.example.org      # the certificate is for this name
export ACME_EMAIL=you@example.org
export PROXY_IP=172.31.7.2              # must match compose.proxy.yml's network
export POSTGRES_PASSWORD="$(openssl rand -hex 24)"
export ADJUTANT_APP_PASSWORD="$(openssl rand -hex 24)"

docker compose -f deploy/compose.proxy.yml up -d --build

# One-off, before the server will boot: create the app role and the plugin roles.
#
# This step must NOT connect as the role it is creating. The compose file points
# the app at `adjutant_app`, so a plain `run --rm adjutant` hands this command a
# database URL for a role that does not exist yet; it fails with
# `password authentication failed for user "adjutant_app"` and never creates it.
# (Observed on a real host, 2026-09-26 — the app then crash-loops on the same
# error, 45 restarts deep, because nothing ever created the role.) Connect as the
# bootstrap superuser instead. `--app-role`/`--app-password` name the role to
# create, so the connection and the role being created are separate things.
docker compose -f deploy/compose.proxy.yml run --rm \
  -e ADJUTANT_DATABASE_URL="postgres://adjutant:${POSTGRES_PASSWORD}@postgres:5432/adjutant" \
  adjutant bootstrap-isolation --app-role adjutant_app --app-password "$ADJUTANT_APP_PASSWORD"

./deploy/verify.sh
```

Requirements: a host with Docker and Compose v2, and DNS pointing at it. For a
certificate without a domain, see `docs/deployment.md` § Putting it behind TLS —
that question has three answers and this directory only assumes the first.

## Status — read this before trusting it

**The bring-up is proven; the three proofs are not.** On 2026-09-26 the stack was
brought up on a real host by following this README, and two things came out of it.

* The `bootstrap-isolation` step **as previously documented could not work**: it
  connected as the role it was creating, so it failed and the app crash-looped on
  the same authentication error. The command above is the corrected one, and it
  ran clean — `core` migrated to version 10, fifteen plugin roles bootstrapped,
  `adjutant_app` created, core schema ownership transferred — after which the app
  booted and served (`listening 0.0.0.0:8787`, 195 routes, and `/` answering
  `{"service":"adjutant","status":"ok"}` from inside the proxy network).
* **`verify.sh` has not run green.** It needs a real `DOMAIN` with DNS and a
  certificate; the host used a placeholder (`adjutant.example.invalid`), so the
  three proofs could not be executed there. Nothing past the bring-up should be
  described as working until `verify.sh` runs green on a host with a real name and
  its output is recorded.

Neither was a fault in the image or the server: the image built and the server has
served correctly on this host since the one change above.

When it does run, the transcript belongs in the PR and a line belongs in
`docs/release-path.md` Stage 3.1 — which is where the deployment host gets chosen,
now that there is something concrete to choose against.
