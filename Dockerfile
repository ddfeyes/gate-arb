# Stage 1: builder — Debian-based to ensure openssl / native-tls works
FROM rust:1.82-slim-bookworm AS builder

# Install build deps for rustls (ring needs cc; libssl for native-tls fallback)
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy workspace manifests first for layer caching
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Stub src so cargo fetch/build can resolve workspace members
RUN mkdir -p src && echo 'fn main(){}' > src/main.rs

# Fetch deps (cache layer)
RUN cargo fetch --locked

# Now copy real source and build
COPY src ./src
RUN cargo build --release --locked --bin gate-arb

# Stage 2: minimal runtime — uses debian slim for ca-certs + healthcheck
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -s /bin/false gate-arb

COPY --from=builder /app/target/release/gate-arb /usr/local/bin/gate-arb

# Health check via curl (available in runtime image)
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl -sf http://localhost:8081/health || exit 1

USER gate-arb
EXPOSE 8080 8081

ENTRYPOINT ["/usr/local/bin/gate-arb"]
