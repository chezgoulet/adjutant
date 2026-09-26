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

Requirements: a host with Docker and Compose v2. DNS pointing at the host is needed
for the **public** path — a certificate for a real name — but not for
`./deploy/verify.sh`, which runs against the origin listener inside the compose
network and therefore needs neither a domain nor TLS. For a certificate without a
domain, see `docs/deployment.md` § Putting it behind TLS — that question has three
answers and this directory only assumes the first.

## Status — read this before trusting it

**The bring-up and all three proofs are proven on a real host** (2026-09-26).
Getting to that point required five corrections to this directory, and they are
listed below, because "authored, not yet proven" turned out to be too generous a
description: the proofs were not *runnable*, and each fix below is why.

* The `bootstrap-isolation` step **as previously documented could not work**: it
  connected as the role it was creating, so it failed and the app crash-looped on
  the same authentication error. The command above is the corrected one, and it
  ran clean — `core` migrated to version 10, fifteen plugin roles bootstrapped,
  `adjutant_app` created, core schema ownership transferred — after which the app
  booted and served (`listening 0.0.0.0:8787`, 195 routes, and `/` answering
  `{"service":"adjutant","status":"ok"}` from inside the proxy network).
* **`verify.sh` runs green.** All three proofs pass on a real host against
  `PROXY_IP=172.31.7.2`:

  ```
  == proof 1 — the proxy is the only way in ==
    ok — no host port is published for adjutant
    ok — the app answers through the proxy
  == proof 2 — the real client is the key, not the proxy ==
    ok — client A's own budget is one request (200 then 429)
    ok — client B has its own budget: the forwarded address is the key
  == proof 3 — a forged header from an untrusted peer buys nothing ==
    ok — rotating a forged header did not produce a second bucket

  all three proofs passed against PROXY_IP=172.31.7.2
  ```

  The proofs talk to the **origin listener inside the network**
  (`THROUGH=http://caddy:8080/api/hello`), so they need no domain, no DNS and no
  certificate. An earlier revision of this file claimed they required a real
  `DOMAIN`; that was wrong and is corrected here rather than left standing.

### Four more things had to be fixed before that could pass

"Authored, not yet proven" was the wrong description. The proofs were **not
runnable** — each of these stopped them before they could test anything:

1. **`curlimages/curl:8` is not a real image tag.** The curator publishes only
   full versions (`8.22.0`, `8.21.0`, …); there is no bare `:8`. The `probe`
   service could therefore never start. Now pinned to `8.22.0`.
2. **Proof 1 failed a correct stack.** It read `docker compose port adjutant 8787`,
   and on Compose v5.5.1 that prints the literal string `invalid IP:0` and exits 0
   for a service that publishes nothing — so a `[ -z ]` test on its output refused a
   configuration that was right. It now reads the rendered configuration
   (`compose config`), which is authoritative and already used by the drift check.
3. **The limiter was never turned down.** `verify.sh` set `ADJUTANT_RATE_MAX` and
   `ADJUTANT_RATE_WINDOW` on the `up` command line, but nothing passed them into the
   container, so the app kept `RateConfig::default` (120 requests / 60s) and proof
   2's `200 then 429` was unobtainable. Both compose files now pass them through,
   defaulting to the app's own values.
4. **"A second `run` is a second client" was false.** Docker returns a freed address
   to the next container: four successive `run`s all came up as `172.31.7.4`, so
   proof 2 could not tell its two clients apart. The probes are now pinned to two
   distinct addresses, and a short limiter window rolls off the readiness probe's
   own request before each proof's client uses that address.

Neither the image nor the server was at fault: the server has served correctly
throughout, and every failure above was in the deployment files or the harness.

The **public TLS path** — a real name, ACME, and the `:443` listener — remains
unproven, because a certificate is not something a placeholder domain can obtain.
`verify.sh` does not test it. The deployment host decision in
`docs/release-path.md` Stage 3 is what that is waiting on.
