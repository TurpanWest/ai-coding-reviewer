# ─── Stage 1: dependency planner ─────────────────────────────────────────────
# cargo-chef computes a "recipe" (the dependency fingerprint) that lets Docker
# cache the compiled dependencies as a separate layer.  When only src/ changes
# that layer is reused, so the full rebuild is avoided.
FROM rust:1-alpine AS chef
RUN apk add --no-cache musl-dev gcc g++ make openssl-dev pkgconf
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ─── Stage 2: builder ─────────────────────────────────────────────────────────
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json

# Build *only* dependencies (this layer is cached as long as Cargo.lock doesn't change)
RUN cargo chef cook --release --recipe-path recipe.json

# Now build the real binary (only src/** is not cached)
COPY . .
RUN cargo build --release

# ─── Stage 3: minimal runtime image ──────────────────────────────────────────
FROM alpine:3

# ca-certificates is required for TLS (reqwest + rustls verify against system roots)
RUN apk add --no-cache ca-certificates

COPY --from=builder /app/target/release/ai-reviewer /usr/local/bin/ai-reviewer

ENTRYPOINT ["ai-reviewer"]
