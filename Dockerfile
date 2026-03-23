# webclaw — Multi-stage Docker build
# Produces 2 binaries: webclaw (CLI) and webclaw-mcp (MCP server)

# ---------------------------------------------------------------------------
# Stage 1: Build all binaries in release mode
# ---------------------------------------------------------------------------
FROM rust:1.93-bookworm AS builder

# Build dependencies: OpenSSL for TLS, pkg-config for linking
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy manifests + lock first for better layer caching.
# If only source changes, cargo doesn't re-download deps.
COPY Cargo.toml Cargo.lock ./
COPY crates/webclaw-core/Cargo.toml crates/webclaw-core/Cargo.toml
COPY crates/webclaw-fetch/Cargo.toml crates/webclaw-fetch/Cargo.toml
COPY crates/webclaw-llm/Cargo.toml crates/webclaw-llm/Cargo.toml
COPY crates/webclaw-pdf/Cargo.toml crates/webclaw-pdf/Cargo.toml
COPY crates/webclaw-mcp/Cargo.toml crates/webclaw-mcp/Cargo.toml
COPY crates/webclaw-cli/Cargo.toml crates/webclaw-cli/Cargo.toml

# RUSTFLAGS (reqwest_unstable) — required by Impit's patched rustls
COPY .cargo .cargo

# Create dummy source files so cargo can resolve deps and cache them.
RUN mkdir -p crates/webclaw-core/src && echo "" > crates/webclaw-core/src/lib.rs \
    && mkdir -p crates/webclaw-fetch/src && echo "" > crates/webclaw-fetch/src/lib.rs \
    && mkdir -p crates/webclaw-llm/src && echo "" > crates/webclaw-llm/src/lib.rs \
    && mkdir -p crates/webclaw-pdf/src && echo "" > crates/webclaw-pdf/src/lib.rs \
    && mkdir -p crates/webclaw-mcp/src && echo "fn main() {}" > crates/webclaw-mcp/src/main.rs \
    && mkdir -p crates/webclaw-cli/src && echo "fn main() {}" > crates/webclaw-cli/src/main.rs

# Pre-build dependencies (this layer is cached until Cargo.toml/lock changes)
RUN cargo build --release 2>/dev/null || true

# Now copy real source and rebuild. Only the final binaries recompile.
COPY crates crates
RUN touch crates/*/src/*.rs \
    && cargo build --release

# ---------------------------------------------------------------------------
# Stage 2: Minimal runtime image
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Copy both binaries
COPY --from=builder /build/target/release/webclaw /usr/local/bin/webclaw
COPY --from=builder /build/target/release/webclaw-mcp /usr/local/bin/webclaw-mcp

# Default: run the CLI
CMD ["webclaw"]
