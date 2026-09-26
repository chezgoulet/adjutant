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
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app

COPY --from=builder /src/target/release/adjutant /usr/local/bin/adjutant
RUN mkdir -p /app/plugins

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
    ADJUTANT_LOG_FORMAT=json
EXPOSE 8787

ENTRYPOINT ["adjutant"]
CMD ["serve"]
