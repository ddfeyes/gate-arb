# Stage 1: builder
FROM rust:1.82-alpine AS builder

# Install build deps
RUN apk add --no-cache musl-dev openssl-dev

WORKDIR /app

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Cache dependencies
RUN cargo fetch 2>/dev/null || true

# Build binary — single release binary with LTO
COPY src ./src
RUN cargo build --release --bin gate-arb \
    && objcopy --strip-debug /app/target/release/gate-arb /app/gate-arb

# Stage 2: runtime — distroless/static base
FROM gcr.io/distroless/static:nonroot

# Create nonroot user
USER nonroot

COPY --from=builder /app/gate-arb /usr/local/bin/gate-arb

# Health check via wget (distroless has it)
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/busybox/wget", "-qO-", "http://localhost:8081/health"]

EXPOSE 8080 8081

ENTRYPOINT ["/usr/local/bin/gate-arb"]
