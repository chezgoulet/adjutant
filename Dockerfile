# syntax=docker/dockerfile:1

# --- build ------------------------------------------------------------------
# Pinned to the MSRV so the container builds with a known-good toolchain.
FROM rust:1.96-bookworm AS builder
WORKDIR /src
COPY . .
# Parallel rustc jobs during the image build. Defaulted low on purpose: a
# self-hosted box is often small, and an uncapped `cargo build --release` on a
# 15 GB host with other stacks running is how a build gets OOM-killed. Raise it
# deliberately where there is room: --build-arg CARGO_BUILD_JOBS=12.
ARG CARGO_BUILD_JOBS=4
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
RUN rustup target add wasm32-wasip1 \
    && cargo build --release --workspace \
    && cargo build --manifest-path wasm/Cargo.toml --release --target wasm32-wasip1

# --- runtime ----------------------------------------------------------------
FROM debian:bookworm-slim AS runtime
# `pg_dump` for the in-app backup button, from PGDG rather than bookworm's own
# repo. That is not a preference: bookworm ships postgresql-client 15, the deploy
# stack runs postgres:18, and `pg_dump` refuses to dump a *newer* server. A
# mismatched client fails with "server version mismatch", which at the admin's end
# would look like the button doing nothing at all — the least diagnosable shape a
# failure can take. Match the client to the server, and if the deployment's
# Postgres ever moves, this line moves with it.
#
# `age` for the in-app backup button's other half: a `pg_dump -Fc` contains every
# plugin role's password in clear text, and compression is not encryption. The
# server encrypts each bundle to ADJUTANT_BACKUP_AGE_RECIPIENT before it reaches
# the backups volume. Only the public key is here — the private key must not live
# on the server, which is the whole point. `age` is in bookworm, so this is a
# package, not a vendored binary.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl gnupg \
    && install -d /usr/share/postgresql-common/pgdg \
    && curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
         -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc \
    && echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] \
https://apt.postgresql.org/pub/repos/apt bookworm-pgdg main" \
         > /etc/apt/sources.list.d/pgdg.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends postgresql-client-18 age \
    && apt-get purge -y --auto-remove gnupg \
    && rm -rf /var/lib/apt/lists/*
# `curl` is deliberately kept: the HEALTHCHECK below is an HTTP probe of the
# server, and the image has no other HTTP client. It is still an unprivileged
# user with no capabilities (see USER/cap_drop), so this is the one binary the
# healthcheck needs rather than a build-time convenience left behind by accident.
WORKDIR /app

COPY --from=builder /src/target/release/adjutant /usr/local/bin/adjutant
# The runtime identity is fixed and documented rather than allocated at build
# time: a volume created by this image inherits these ids, and an operator
# upgrading an existing backup volume can name the same pair when chowning it.
# 10001 is above Debian's system range (100-999), so the base image will not
# assign it to a package account on a later rebuild.
RUN groupadd --gid 10001 adjutant \
    && useradd --uid 10001 --gid adjutant --no-create-home --home-dir /app \
         --shell /usr/sbin/nologin adjutant \
    && mkdir -p /app/plugins /app/backups \
    && chown -R adjutant:adjutant /app

# Every plugin library the workspace built, by the same rule
# `scripts/stage-plugins.py` uses: a plugin is a workspace crate that produces a
# cdylib, and the SDK is an rlib, so it is excluded by that rule rather than by
# being named. The glob is the point.
#
# This line used to name three libraries — hello, auth, membership — and the
# workspace builds fourteen. That is the same drift issue #55 fixed for the
# staging and `validate-plugin` lists, in the one place the fix did not reach:
# a one-command deployment whose server carries three plugins cannot answer the
# client's finance, governance, missions, calendar, store, announcements,
# archive, conflicts, equipment, mcp or stripe screens at all, and it fails at
# the screen rather than at the boot, which is the worst place to find out.
COPY --from=builder /src/target/release/libadjutant_*.so /app/plugins/
COPY --from=builder /src/wasm/target/wasm32-wasip1/release/adjutant_hello_wasm.wasm /app/plugins/

# Dev identity headers stay OFF unless explicitly opted in (SPEC §7.1).
ENV ADJUTANT_PLUGIN_DIR=/app/plugins \
    ADJUTANT_BIND=0.0.0.0:8787 \
    ADJUTANT_BACKUP_DIR=/app/backups \
    ADJUTANT_LOG_FORMAT=json
EXPOSE 8787

# Everything above that writes happens as this user. The process never needs a
# capability: it binds 8787 (>1024) and spawns `pg_dump`/`age` as children. The
# compose files assert that too (`cap_drop: [ALL]`, `no-new-privileges`), so a
# future image change that wants root has to say so rather than inherit it.
USER adjutant:adjutant

# Probe `/`, the core health route: it needs no plugin, no session and no
# database read, so it answers exactly while the process is able to serve. It is
# also one of the two open probes named in deploy/traefik/adjutant.yml.
#
# The interval is comfortably wider than the rate limiter's window in every
# shipped configuration (deploy/verify.sh deliberately narrows it to 1 request
# per 5s): a probe that exhausts its own budget would report 429 and teach
# operators to ignore a red light.
#
# `start-period` is long on purpose. The listener is bound only after core and
# plugin migrations finish, and migration 17 rewrites the whole audit chain, so
# a large installation can legitimately serve nothing for minutes. Failures
# inside this window do not count toward `retries`.
#
# `NotBootstrapped` is deliberately RED. The process refuses to boot before
# `bootstrap-isolation` and exits 1, so the port is never bound and the probe
# cannot succeed; the container's health state is `unhealthy` instead of the old
# image's bare "Up" with no health state at all. (A crash-loop that exits in
# under a second makes compose print `Restarting` rather than `Up (unhealthy)`,
# but it is never healthy.) That is the defect this healthcheck exists to
# expose, not a false alarm: pre-bootstrap is a required setup step that has not
# been run, not a serving state. A green light there is precisely what hid the
# crash-loop.
HEALTHCHECK --interval=15s --timeout=5s --start-period=5m --retries=3 \
  CMD curl -fsS --max-time 4 http://127.0.0.1:8787/ || exit 1

ENTRYPOINT ["adjutant"]
CMD ["serve"]
