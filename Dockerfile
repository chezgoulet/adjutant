# syntax=docker/dockerfile:1

# --- build ------------------------------------------------------------------
# Pinned to the MSRV so the container builds with a known-good toolchain.
FROM rust:1.88-bookworm AS builder
WORKDIR /src
COPY . .
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
COPY --from=builder /src/target/release/libadjutant_hello.so /app/plugins/
COPY --from=builder /src/target/release/libadjutant_auth.so /app/plugins/
COPY --from=builder /src/target/release/libadjutant_membership.so /app/plugins/
COPY --from=builder /src/wasm/target/wasm32-wasip1/release/adjutant_hello_wasm.wasm /app/plugins/

# Dev identity headers stay OFF unless explicitly opted in (SPEC §7.1).
ENV ADJUTANT_PLUGIN_DIR=/app/plugins \
    ADJUTANT_BIND=0.0.0.0:8787 \
    ADJUTANT_LOG_FORMAT=json
EXPOSE 8787

ENTRYPOINT ["adjutant"]
CMD ["serve"]
