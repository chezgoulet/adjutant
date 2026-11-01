# Deployment

How to run Adjutant, upgrade it safely, and back it up. See the
[README](../README.md) for the environment variable reference and the
[plugin guide](plugin-development.md) for plugin development.

Adjutant is a single Rust binary plus plugin `.so` files and a PostgreSQL
database. There is no Redis, no MinIO, and no separate cache (SPEC §2.6).

---

## Quick start (Docker Compose)

```bash
# 1. Pick both passwords (required — there are no insecure defaults).
export POSTGRES_PASSWORD="$(openssl rand -hex 24)"       # admin; bootstrap only
export ADJUTANT_APP_PASSWORD="$(openssl rand -hex 24)"   # the server's runtime role

# 2. Bootstrap, then start.
#    `bootstrap-isolation` is a ONE-OFF that creates the per-plugin roles,
#    schema ownership, `core.*` grants and credentials, AND the `adjutant_app`
#    runtime role. Three things to know:
#      * it must run BEFORE the server;
#      * it needs the plugin directory present — it iterates what is on disk;
#      * re-run it whenever a plugin is added or upgraded.
#    An un-bootstrapped plugin makes the server refuse to load it
#    (`NotBootstrapped`, exit 1) — that is the expected state until this step.
docker compose run --rm \
  -e ADJUTANT_DATABASE_URL="postgres://adjutant:${POSTGRES_PASSWORD}@postgres:5432/adjutant" \
  adjutant bootstrap-isolation --app-role adjutant_app --app-password "$ADJUTANT_APP_PASSWORD"
docker compose up -d

# 3. Check it is up
docker compose ps                # adjutant: Up (healthy) once the first probe passes
curl -s localhost:8787/          # {"service":"adjutant","status":"ok"}
```

Skipping step 2 leaves the container **`Restarting`** (a crash-loop is fast) with
`Health=unhealthy`, never a bare `Up`: before `bootstrap-isolation` the server
exits with `NotBootstrapped` and never binds the port, so the probe cannot
answer. See [Container user and health](#container-user-and-health).

The server connects as **`adjutant_app`**, a non-superuser role (no
`SUPERUSER`, no `CREATEROLE`); it is never the Postgres superuser. On start it
runs core migrations, loads the bundled plugins (`hello`, `auth`, `membership`),
and runs each plugin's migrations — **on a pool authenticated as that plugin's
own `adjutant_plugin_<id>` role**.

Postgres is **not** exposed to the host; only the server is.

**The first chief comes from one of two paths, never both.**

- **Environment (recommended).** Set `ADJUTANT_BOOTSTRAP_USERNAME` and
  `ADJUTANT_BOOTSTRAP_PASSWORD` (and optionally `ADJUTANT_BOOTSTRAP_EMAIL`) in
  the server's environment. On the first boot against a database with no users,
  the server creates that account with the `chief` role before it opens the
  listener. Set both variables or neither: half a pair refuses to boot rather
  than leaving a server nobody can log in to. Once the database has users the
  variables are ignored on every later boot — changing the password there does
  **not** change the account, because an environment that can rewrite a password
  on restart is a backdoor. Remove the password from the environment after the
  first boot.
- **First registration.** `POST /api/auth/register` is open only while no user
  exists; the first caller becomes `chief`. The one bootstrap slot is a row
  (`core.bootstrap_claim`), so two concurrent first registrations cannot both
  win: exactly one gets `201`, the other `403`.

`adjutant grant-role <username> <role>` is the operator's recovery hatch. It
writes `core.user_roles` directly, so it works when the last admin has been
removed and the API is unreachable.

**Upgrading an existing deployment:** re-run `bootstrap-isolation` against the
new binary before starting the server. The auth plugin holds a new, narrow grant
on `core.bootstrap_claim`; without it, registration returns `500` rather than
`403` on an installation that has not been re-provisioned.

## The application role

`adjutant_app` replaces the old "run as `POSTGRES_USER`" default, which was a
superuser and turned any plugin escape into total compromise (design
`plugin-isolation.md` §3.6). It is:

- `LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE`;
- a member of every `adjutant_plugin_<id>` role (it administers them);
- the **owner of the `core` schema** and its objects, and granted `CONNECT,
  CREATE` on the database.

That last point is the one deviation from the design's "owns nothing": because
core migrations run at boot and `ALTER` core tables, the role that serves must
own `core` — otherwise the only role that could boot would be a superuser. It
owns **no plugin schema**; those belong to the plugin roles. The server refuses
to boot on a superuser connection unless `ADJUTANT_ALLOW_SUPERUSER=true` is set
for a throwaway database.

## Container user and health

The image runs the server as a fixed unprivileged identity, **uid/gid
10001:10001** (`adjutant`). Both compose files repeat `user: "10001:10001"` so
`docker inspect` cannot disagree with the Dockerfile. Everything the process
writes is owned by that pair in the image: `/app` (with `/app/plugins` and
`/app/backups`). A *new* named `backups` volume inherits the ownership of the
image directory it mounts over, so a fresh deployment needs no chown.

**Upgrading a stack created before this change.** The existing `backups` volume
was initialised while the image still ran as root, so it is root-owned and the
in-app backup button will fail with a permission error. Fix it once, before or
after starting the new image:

```bash
docker compose run --rm --user root --cap-add CHOWN --entrypoint chown adjutant \
  -R 10001:10001 /app/backups
```

`--cap-add CHOWN` is required, not optional: the service sets
`cap_drop: [ALL]`, so even uid 0 cannot change an owner without it. (That is the
hardening working, and it is why the command names the one capability it needs
rather than running the service without `cap_drop`.)

The container's `/` is not writable by design; do not work around a permission
error by putting `user: root` back. Name the path that failed — the fix is the
ownership of that path.

**The healthcheck** is defined in the image (so `docker run` has it too) and
probes `GET http://127.0.0.1:8787/` — the core, unauthenticated health route,
which needs no plugin, session or database read:

| | |
|---|---|
| interval | 15s |
| timeout | 5s |
| start-period | 5m |
| retries | 3 |

The start-period is generous because the listener is bound only **after** core
and plugin migrations finish, and migration 17 rewrites the whole audit chain;
a large installation can legitimately serve nothing for minutes. Failures
during the start-period do not count toward the retries.

**Pre-bootstrap is deliberately red.** `NotBootstrapped` is an *expected*
state — but it is expected only because a required setup step has not been run.
The process exits 1 and never binds the port, so the probe cannot succeed and
the container carries `Health=unhealthy`. The old image had no healthcheck, so
the same container could be sampled as plain `Up` between restarts, with no
health state at all, and an operator watching status saw nothing wrong.

Be precise about what a healthcheck can show for a *fast* crash-loop: Docker
only runs the probe while the container is running, and this one exits in well
under a second, so `docker compose ps` shows `Restarting` rather than
`Up (unhealthy)`. The health state is `unhealthy` all the same (`docker inspect
… .State.Health.Status`), and a process that stays alive without binding the
port — a long migration, a hung dependency — does show `Up (unhealthy)` after
the start-period and retries. Either way it is never `healthy`. The health
light answers "is this container serving?", not "did it start?". Once
`bootstrap-isolation` has run and the server answers, the state becomes
`healthy` within one interval.

The interval is wider than the rate limiter's window in the shipped
configurations (including `deploy/verify.sh`, which narrows the limiter to one
request per 5s) so the probe never reports its own 429 as a dead server.

**Hardening.** The `adjutant` service drops all capabilities
(`cap_drop: [ALL]`) and sets `no-new-privileges:true` in both compose files: it
binds 8787, and its `pg_dump`/`age` children need no capability. `postgres`
keeps its default capabilities because `initdb` and the entrypoint's drop to
the `postgres` user need CHOWN/SETUID/SETGID/DAC_OVERRIDE/FOWNER, but it also
sets `no-new-privileges:true` — it never needs to *gain* a privilege.

## Bare metal

```bash
cargo build --release --workspace          # produces target/release/adjutant + *.so
mkdir -p plugins-built
cp target/release/libadjutant_*.so plugins-built/

# One-time, and again after adding/upgrading a plugin: create the roles,
# schema ownership, `core.*` grants and stored credentials, plus the app role.
# Needs CREATEROLE (the admin role).
ADJUTANT_DATABASE_URL=postgres://adjutant@127.0.0.1:5432/adjutant \
  ./target/release/adjutant bootstrap-isolation --plugin-dir plugins-built \
    --app-role adjutant_app --app-password "$ADJUTANT_APP_PASSWORD"

# Serve as the non-superuser app role. The runtime connects as each plugin role
# for plugin SQL; it never needs CREATEROLE.
ADJUTANT_PLUGIN_DIR=plugins-built \
ADJUTANT_DATABASE_URL=postgres://adjutant_app:"$ADJUTANT_APP_PASSWORD"@127.0.0.1:5432/adjutant \
  ./target/release/adjutant serve
```

`bootstrap-isolation` needs a role that can `CREATE ROLE`, `CREATE EXTENSION`
(`pgcrypto`), and create the `core` schema (it runs core migrations). The
**runtime** role needs only `CONNECT` plus the `core.*` privileges core services
use; it does **not** need `CREATEROLE`. Each plugin's SQL runs as its own
`adjutant_plugin_<id>` role, which owns one schema and has only its explicit
`core.*` allowlist. Passwords are generated by `bootstrap-isolation`, stored in
`core.plugins.db_secret` (never in `config`, which plugins receive), and are
preserved on re-run unless `--rotate` is passed.

```sql
CREATE ROLE adjutant LOGIN CREATEROLE;
CREATE DATABASE adjutant OWNER adjutant;
```

## Per-plugin pools and the connection budget

Each loaded plugin gets its own `PgPool` (2 connections by default), alongside
the core's pool. The rough total is:

    core pool (10) + loaded plugins × 2

PostgreSQL's `max_connections` defaults to 100, so raise it once you run more
than ~45 plugins. A hot-reload builds fresh pools for the new generation while
retired plugins keep theirs, so leave reload headroom. See
[`design/plugin-isolation.md`](design/plugin-isolation.md) §3.3.

## The audit log and `ADJUTANT_AUDIT_KEY`

`core.audit_log` is append-only and hash-chained. Each row's `entry_hash` is
SHA-256 over `prev_hash | id | action | resource_type | resource_id |
details::text | source | user_id | created_at` — so changing **who** or **when**
a row records breaks the chain, not only its action and details. That chain is
recomputed inside PostgreSQL and is checked by `GET /api/audit/verify` (`ok`,
`first_bad`, `rows_checked`) and by `adjutant audit-verify`.

**Migration 17 costs time proportional to `core.audit_log`.** It disables the
append-only trigger and rewrites every existing row in one transaction to
recompute the v2 chain, so upgrade time and lock duration scale with the table's
row count. A large deployment should expect an upgrade window. Any error rolls
the whole migration back, leaving the old chain intact and the trigger armed.

The unkeyed chain is tamper-**evidence** only against a writer who cannot
recompute it. The role that serves Adjutant **owns** `core.audit_log`, so it can
disable the triggers, rewrite a row, recompute the chain and verify `ok:true`.
To detect that, set a key the database does not hold:

```bash
export ADJUTANT_AUDIT_KEY="$(openssl rand -hex 32)"   # 64 hex chars, or base64 of 32 bytes
```

On every insert the server computes `keyed_hash = HMAC-SHA256(key, the same
canonical payload)` **before** the INSERT, in the same transaction that takes the
chain lock. The key is never sent to PostgreSQL, never written to the database,
and never logged (its `Debug` is redacted). A malformed value refuses boot with
an `ADJUTANT_AUDIT_KEY:` error rather than silently running unkeyed.

**The honest threat model.**

- **Protects against:** someone who can read or write the database — including
  the role that owns `core.audit_log`, or a stolen backup, or `pg_dump` — but
  does not hold the server's environment. Rewriting a row and recomputing the
  unkeyed chain leaves `keyed_hash` stale; the app verifier names the row
  (`keyed_first_bad`). A restored dump cannot be re-signed without the key.
- **Does not protect against:** a compromised server process. It holds the key
  to write each row, so an attacker with code execution or the environment can
  compute valid hashes. The key is an anchor against the database, not against
  the application.
- **It is not an external notarisation.** There is no periodic export to a store
  the database cannot write; an attacker who compromises both the database and
  the server's environment is outside this layer's reach. A database owner can
  also strip `keyed_hash` from rows — `keyed_rows_unkeyed` makes that visible,
  but a full strip leaves no keyed row to compare, so the honest longer-term
  answer is an exported `(id, keyed_hash)` snapshot somewhere the database
  cannot write. That is not built here.

**Losing the key is permanent for the keyed layer.** Past `keyed_hash` values
cannot be re-verified without the original key (an HMAC is not reversible, and
there is no second copy). Keep the key with the other deployment secrets (for
example in `deploy/.env`, which the backup dump deliberately excludes). The
unkeyed chain still verifies after a key loss; `/api/audit/verify` will simply
report `keyed_first_bad` on every row that was keyed, and keying new rows under a
different key would start a new keyed generation. **Do not rotate the key in
place**: rows under the old key can never match the new one.

**Rotation is restart-shaped.** `set_chain_key` installs the key in a
`OnceLock`: the first call wins, and a second call — including one carrying a
different value — silently returns `false` and leaves the first key in force. A
changed `ADJUTANT_AUDIT_KEY` therefore has no effect until the process restarts.
The restart is what rotates the key; rows written before it remain under the old
key, so plan the same way as any other key change rather than expecting an
in-process switch.

Without a key, the column stays NULL, boot logs a WARN naming what is not
protected, and both the HTTP route and `adjutant audit-verify` report
`keyed: false`. That is a statement about the anchor, not a green tick.

To verify from a shell (exit code 1 if either layer fails):

```bash
adjutant audit-verify --database-url "$ADJUTANT_DATABASE_URL"
```

## Putting it behind TLS

Terminate TLS at a reverse proxy (Caddy, nginx, Traefik) and forward to the
server's port. **The application does not terminate TLS and will not** — it binds
plain HTTP and reads exactly one header from the proxy. A working stack, its
config for both Caddy and Traefik, and three executable proofs of the properties
below live in [`deploy/`](../deploy/README.md).

Two settings matter:

- `ADJUTANT_TRUSTED_PROXIES` — comma-separated proxy IPs allowed to set
  `x-forwarded-for`. With this set, rate limiting keys on the real client;
  otherwise the header is ignored and every request behind the proxy shares one
  bucket.
- `ADJUTANT_CORS` — comma-separated allowed browser origins (`*` for any). Empty
  means same-origin only.

**Keep `ADJUTANT_DEV_HEADERS` off** (the default). When on, anyone who can reach
the port can claim any role by setting `x-dev-user`/`x-dev-role`. A deployment
that inherits `allow_dev_headers = true` from a config file can force it off on
the command line with `--no-dev-headers`; the boot log warns while it is on, and
`GET /api/plugins` reports `dev_headers.enabled` (with the risk text) so an
auditor reading the admin surface sees it too.

The session cookie is always `HttpOnly; SameSite=Lax; Path=/` and, by default,
`Secure`. A browser will not send a `Secure` cookie back over plain HTTP, so
local development on `http://127.0.0.1` needs
`ADJUTANT_INSECURE_COOKIES=true` to log in. That flag is **development-only**:
leave it unset behind TLS. It is never inferred from `ADJUTANT_BIND` or from a
forwarded header — a deployment that terminates TLS at the proxy must not set
it.

Three things about the trust setting that are easy to get wrong, and are the
reason `deploy/verify.sh` exists:

- **It is a literal address comparison against the direct peer.** On a container
  network the proxy's address is usually dynamic, so a hardcoded value silently
  stops matching — and the failure is quiet: the header is ignored and every
  client shares one bucket. Give the proxy a static address (`deploy/compose.proxy.yml`
  pins one on a fixed subnet) and keep the two values equal.
- **The default is empty, which ignores the header entirely.** That is the safe
  direction: honouring `x-forwarded-for` from anyone lets any client reset its own
  rate-limit budget by rotating one header value. The list is walked from the
  right, skipping trusted hops, so a client-injected entry on the left is never
  used.
- **The address must match the bind family.** `ADJUTANT_BIND=0.0.0.0:8787` gives
  dotted-quad peers; binding the wildcard IPv6 address can present a v4 peer as
  `::ffff:172.x.y.z`, which never string-matches a dotted-quad entry. Fail-safe,
  but wrong.

**And the port itself:** do not publish the application's port when it is behind a
proxy. An app reachable directly is an app reachable without TLS, whatever the
proxy is doing — that is proof 1 in `deploy/verify.sh`.

**The client has an opinion about this too.** A released build of the phone app
reaches `https` only: Android has refused cleartext HTTP by default since API 28,
and the app relaxes that for debug builds alone (`android/app/src/debug/…`). So a
troop whose server is plain `http://` on the LAN has a working server and an app
that cannot talk to it. Every option below serves https, which is why the client
can insist on it — but if you are deploying plain HTTP on purpose, know that the
app will refuse the address and say so on the sign-in screen rather than failing
as an unreachable server.

### A troop with no domain name

**The phone app needs one of the first two**, and it is worth understanding why
before choosing the third.

The app reaches `https` only, and it trusts exactly what the operating system
trusts. A server whose certificate comes from a troop's own certificate authority
is therefore one the app cannot verify: the phone has never heard of that CA. That
is not a gap to be worked around with a setting — the way around it would be to
teach the app to accept a certificate it cannot validate, which is the same as
accepting *any* certificate for that address, and that is worse than not connecting.
Per-troop pinning (the app trusting one fingerprint, for one address, chosen by
the admin) is the feature that would change this. It is a trust decision with its
own interface, it is not built, and it is not needed for either supported path.

1. **Own a name** (or a subdomain of one) and let the proxy obtain a certificate
   automatically — Caddy does this with no cron job and nothing to renew by hand.
   This is the path `deploy/Caddyfile` takes.
2. **Use a tunnel to a hosted name** — the shape The House runs: Cloudflare Tunnel
   terminates TLS at the edge and forwards plain HTTP to the origin. The origin
   listener in `deploy/Caddyfile` (`:8080`, on the private network only) exists for
   exactly this, and `ADJUTANT_TRUSTED_PROXIES` then names the tunnel container.
3. **LAN-only with your own certificate authority.** Browsers will not trust it
   without installing the CA on every device, which for a troop means every phone.
   Workable, and honestly the worst of the three for a group that has to
   self-serve. **A troop that takes this path is choosing a browser client on the
   LAN**, and should know that is the choice: the phone app will not connect, and
   it will say so on the sign-in screen rather than pretending the address is
   unreachable.

Neither of the first two costs money — a subdomain and a Cloudflare tunnel are
both free — which is why the app can reasonably insist on them rather than
accommodate an untrusted certificate.

## Upgrading

Adjutant applies migrations automatically at boot, so an upgrade is: stop, swap
the binary/plugins, start.

```bash
# 1. Back up first (see below).
# 2. Get the new release (binary + plugins), then:
docker compose down
docker compose build --pull
docker compose up -d
```

Notes:

- **Upgrading to the per-plugin isolation release:** run
  `adjutant bootstrap-isolation --plugin-dir <dir>` once against the database
  before starting the new binary. It creates the `adjutant_plugin_<id>` roles
  and credentials and transfers ownership of existing plugin schemas/objects to
  those roles. The new runtime refuses to load a plugin with no credential.
  Add `--app-role adjutant_app --app-password <pw>` to also create the runtime
  role and hand it the `core` schema, then serve as it. Re-run it after any
  upgrade that adds a core grant (e.g. the `core.scope_hierarchy` grant
  membership needs to declare its lodge→patrol hierarchy): `bootstrap-isolation`
  re-asserts the allowlist on every run. It revokes the role's existing table-
  and column-level privileges on `core.*` first, so a re-run also removes
  authority an older release granted and this one does not — including the
  table-level `SELECT` on `core.users` that predates column-level grants.
- **The runtime now refuses to boot on a superuser connection** (a plugin escape
  would be total compromise). Serve as the non-superuser app role, or set
  `ADJUTANT_ALLOW_SUPERUSER=true` for a throwaway database.
- Core migrations and plugin migrations are recorded in
  `core.schema_migrations` and run once, in order. They are additive; there are
  no down-migrations, so **restore from backup is the rollback path**.
- Plugin `.so` files must be rebuilt against the same SDK ABI as the core. A
  stale plugin is refused at load with an explicit ABI-mismatch error (run
  `adjutant validate-plugin <so>` to check before deploying).
- If a release bumps the SDK ABI, rebuild and re-copy all plugins.

## Backup and restore

**Everything Adjutant knows lives in PostgreSQL** — the `core` schema, every
plugin's schema, the roles' grants, and the append-only audit chain. So the
database dump is the backup, and `deploy/backup.sh` takes it:

```bash
export ADJUTANT_BACKUP_AGE_RECIPIENT="age1..."   # public key, safe on the server
./deploy/backup.sh                 # or: ./deploy/backup.sh /other/dir
```

| | |
|---|---|
| **What** | the `adjutant` database (`pg_dump -Fc`), encrypted with `age`, plus its `.sha256` |
| **Artifact** | `adjutant-<stamp>.dump.age` (a deliberate plaintext fallback is `.dump`) |
| **Where** | `$ADJUTANT_BACKUP_DIR`, default `/var/backups/adjutant` |
| **Retention** | `$ADJUTANT_BACKUP_KEEP_DAYS`, default 14 days, pruned by the script |
| **Cadence** | daily, and immediately before any upgrade — see below |
| **NOT in the dump** | `deploy/.env` (it holds `POSTGRES_PASSWORD` and `ADJUTANT_APP_PASSWORD`) and the plugin `.so` files. Back the `.env` up somewhere else and keep it: without it a restore cannot reproduce the credentials the roles were created with. |

The script refuses to call a zero-byte dump a success. An encrypted bundle is
checked for the age header, and when `ADJUTANT_BACKUP_AGE_IDENTITY` is available
it is decrypted and re-read with `pg_restore -l` — a dump you cannot list is a
dump you cannot restore.

### A bundle is credential material

`core.plugins.db_secret` holds every plugin role's password, and `pg_dump -Fc`
is compressed, **not encrypted**. Every bundle — host-produced or from the
in-app backup button — is encrypted to an age **recipient** public key before it
is written. `pg_dump`'s stdout is piped straight into `age` in both producers,
so the complete credential-bearing archive exists only inside that pipe: no
plaintext dump is written to the backups volume (or anywhere else), not even a
`0600` temporary one that a crash or an OOM kill could leave behind.

| Variable | Meaning |
|---|---|
| `ADJUTANT_BACKUP_AGE_RECIPIENT` | age public key (`age1...`) used to encrypt. Safe on the server; the matching **private** key must not be. |
| `ADJUTANT_BACKUP_AGE_IDENTITY` | path to the age identity file, needed only to **restore** or to read a bundle back. |
| `ADJUTANT_BACKUP_ALLOW_PLAINTEXT` | the explicit opt-out. Must be `true` (or `1`) to write a plaintext `.dump` and log a WARN. |

**The default is fail closed.** With no recipient configured a backup fails with
an error naming `ADJUTANT_BACKUP_AGE_RECIPIENT`; it does not quietly write a
plaintext bundle. `ADJUTANT_BACKUP_ALLOW_PLAINTEXT=true` is the one deliberate
exception, and the WARN it logs says the bundle is credential material.

Generate a key pair somewhere that is **not** the deployment host:

```bash
age-keygen -o adjutant-backup.key       # keep this off the server
grep '^# public key:' adjutant-backup.key
```

`deploy/backup.sh` and `deploy/restore.sh` use `age` from the host when it is
installed there, and otherwise run it from the runtime image; the in-app backup
button always uses the image's copy. The Dockerfile's runtime stage installs it
for exactly this reason.

**What encryption does and does not protect.** The bundle lives in a Docker
named volume on the same host as the database, so encrypting it protects the
copy, not the host. A host compromise still reaches the running database and
everything the app can read; age turns a stolen bundle or an accidentally
copied volume from "every plugin password and all troop data" into ciphertext.
Treat the identity file as the recovery key: without it the bundle is
unreadable, and losing it is losing every backup.

### Restoring

```bash
export ADJUTANT_BACKUP_AGE_IDENTITY=/root/adjutant-backup.key
./deploy/restore.sh /var/backups/adjutant/adjutant-2026-10-20-120000.dump.age --yes
```

`--yes` because it drops the current database. A `.dump.age` with no identity
configured, or with an identity file that does not exist, **fails before the
database is dropped** — never drop a database you cannot restore. The script
then validates the archive before dropping anything. The decrypt is piped
straight into `pg_restore -l` for that check and into `pg_restore` for the
restore itself, so **no plaintext dump is written to disk at any point**; the
bundle is decrypted twice rather than leaving a clear-text copy behind. It then
stops the app, recreates the database, runs `pg_restore --no-owner --no-acl` (**no** ACLs or
owners yet, because the roles do not exist), runs `bootstrap-isolation`, and
finally runs `adjutant verify-isolation`. Only then does it start the app and
refuse to exit successfully unless it answers through the proxy and the plugin
grants exist.

`verify-isolation` is the step that makes `ok` mean something. For every
installed plugin it checks that the `adjutant_plugin_<id>` role exists, that the
plugin's schema and tables are owned by it, that its `core.*` allowlist grants
are present and no unlisted `core.*` table is reachable, and that a connection
made the way the runtime makes it reports `session_user` as that role. It also
checks `adjutant_app` exists, owns `core`, and is a member of every plugin role.
Failures name the specific missing role or table. A plaintext `.dump` from an
older release is still accepted, with a WARN that it is credential material.

### The step everyone misses, and why it is the whole reason there is a script

A `pg_dump` of a database contains the database. It does **not** contain the
cluster's roles, because roles are cluster-level objects — and Adjutant runs
every plugin as its own role (`adjutant_plugin_<id>`) and the core as
`adjutant_app`. Restoring a dump onto a clean host therefore produces a database
whose grantee roles do not exist. Observed, on the first drill:

```
roles once a CLEAN postgres is up: 1        <- only `adjutant`
pg_restore: error: could not execute query: ERROR:  role "adjutant_plugin_auth" does not exist
Command was: GRANT SELECT,INSERT,DELETE ON TABLE core.sessions TO adjutant_plugin_auth;
pg_restore: warning: errors ignored on restore: 6
```

**`errors ignored` is the dangerous part.** The old script reported success, the
data was all there, and the server then crash-looped on

```
password authentication failed for user "adjutant_app"     (28P01, 7 restarts, exit 1)
```

which points at a password when the actual fault is a role that was never
created. `bootstrap-isolation` closes it: it creates the roles, transfers
ownership of the plugin schemas to them, hands the `core` schema to
`adjutant_app`, stores each plugin's credential, and **re-asserts the grant
allowlist** — which is what puts back the six `GRANT`s the old restore dropped,
without ever attempting them against roles that do not exist.

### Measured, not asserted

Restore drill, 2026-09-26, on a real host, following the scripted procedure from a
destroyed volume:

| | |
|---|---|
| dump size / time | 242,369 bytes in **1s** |
| `pg_restore` | 2s |
| `bootstrap-isolation` | 3s |
| **`restore.sh` wall clock** | **6s** |
| after: marker row | present |
| after: `core.plugins` / `core.permissions` | 15 / 60 |
| after: roles in the cluster | 17 |
| after: plugin-role grants | 335 |
| after: owner of `hello.greetings` / `core` | `adjutant_plugin_hello` / `adjutant_app` |
| after: the app | `running`, answering through the proxy |

**RTO** is therefore ~6 seconds *for this dataset* — which is schema and
provisioning, with no troop data in it. Do not read that as the RTO for a troop
that has been running for a year: the dump grows with content, and the honest
statement is "seconds of mechanical time plus the time to move the dump", not a
fixed number.

**RPO** is the backup interval, and the interval is what the cadence above
decides — nothing is taken automatically yet. Daily means up to a day of loss.
Automating it (a host cron or systemd timer calling `backup.sh`) belongs with the
deployment host decision in `docs/release-path.md` Stage 3, and until that exists
the RPO is whatever the operator actually ran.

> A physical volume snapshot (`pgdata`) also works but requires a consistent
> Postgres shutdown. `pg_dump` is the portable option.

The audit log is append-only and hash-chained, so it is the one thing a partial
restore can quietly corrupt. Verify it after a restore with
`GET /api/audit/verify` (admin only) or `adjutant audit-verify`. Check both
layers: `ok` is the unkeyed chain and `keyed` is whether the HMAC layer was
checked. A restore cannot recompute `keyed_hash` values — that is the point of
the key — so a dump restored and then tampered with will fail the keyed layer
even if its unkeyed chain was rebuilt.

## Releases

Tagged releases (`v*`) publish a tarball containing the `adjutant` binary, the
bundled plugin `.so` files, `README.md`, and `LICENSE` (see
`.github/workflows/release.yml`). Verify a plugin before loading it:

```bash
adjutant validate-plugin libadjutant_your_plugin.so
```
