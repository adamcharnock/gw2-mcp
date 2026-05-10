# syntax=docker/dockerfile:1.6
#
# Multi-stage build using cargo-chef to cache dependency compilation.
# Final image is distroless (~25MB) and runs as a non-root user.

FROM rust:1.91-slim AS chef
# `assets/` includes a vendored JSON file that gets compiled into the
# binary via include_str!; copy it alongside src/ in every COPY below.
#
# OpenSSL headers + pkg-config are needed because chatr's transitive
# reqwest 0.11 dependency uses the openssl-sys default TLS stack on
# Linux. Our own reqwest 0.12 dep uses rustls-tls and would not need
# this on its own.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
COPY tests ./tests
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
# Cache dependency compilation in a separate layer.
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Now build the actual binary.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release --bin gw2-mcp \
    && strip target/release/gw2-mcp

# Runtime image — distroless, non-root, no shell.
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=builder /app/target/release/gw2-mcp /usr/local/bin/gw2-mcp

LABEL org.opencontainers.image.title="gw2-mcp"
LABEL org.opencontainers.image.description="Guild Wars 2 MCP server"
LABEL org.opencontainers.image.source="https://github.com/adamcharnock/gw2-mcp"
LABEL org.opencontainers.image.licenses="AGPL-3.0-or-later"

USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/gw2-mcp"]
