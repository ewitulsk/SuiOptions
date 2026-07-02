# syntax=docker/dockerfile:1
# Enclave app image for bridge-signer-service (bridge_tickets/07).
#
# Build context = the `rust-backend` workspace root (the signer is a workspace
# member and needs the workspace Cargo.toml/lock to build):
#   docker build -f bridge-enclave/Dockerfile -t bridge-signer-enclave .
#
# SCAFFOLD: this builds the plain signer service. Ticket 07 Phase 1 replaces the
# entrypoint with the nautilus-server (vsock + attestation + in-enclave TLS).
#
# NOT YET bit-reproducible — Phase 2 hardening TODO:
#   - pin the base images BY DIGEST (FROM rust:1.90-slim@sha256:...) not by tag
#   - set SOURCE_DATE_EPOCH, strip timestamps, cargo --locked (already below)
# PCR0 is only meaningful once this is deterministic.

# ---- builder ----
FROM rust:1.90-slim AS builder
# TODO(ticket-07): pin by digest for reproducibility.

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

# --locked: build exactly the committed Cargo.lock (reproducibility + no surprise bumps).
RUN cargo build --locked --release -p bridge-signer-service \
    && strip target/release/bridge-signer-service

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
# TODO(ticket-07): pin by digest; consider distroless for a smaller/measured surface.

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/bridge-signer-service /usr/local/bin/bridge-signer-service

# The signer reads its config from a path the enclave provisions. Inside a real
# Nitro enclave the nautilus-server (Phase 1) is PID 1 and starts this over vsock;
# for the scaffold image the binary is the entrypoint.
ENTRYPOINT ["/usr/local/bin/bridge-signer-service"]
CMD ["--config", "/etc/bridge-signer/config.toml"]
