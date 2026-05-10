# syntax=docker/dockerfile:1.6
#
# Multi-stage build using cargo-chef to cache dependency compilation.
# Final image is distroless (~25MB) and runs as a non-root user.

FROM rust:1.91-slim AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
# Cache dependency compilation in a separate layer.
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Now build the actual binary.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
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
