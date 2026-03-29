# ─── Stage 1: dependency planner ─────────────────────────────────────────────
FROM rust:1-slim-bookworm AS chef
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev gcc g++ make \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ─── Stage 2: builder ─────────────────────────────────────────────────────────
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json

# Build *only* dependencies (cached as long as Cargo.lock doesn't change)
RUN cargo chef cook --release --recipe-path recipe.json

# Build the real binary (only src/** is not cached)
COPY . .
RUN cargo build --release

# ─── Stage 3: minimal runtime image ──────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/ai-reviewer /usr/local/bin/ai-reviewer

ENTRYPOINT ["ai-reviewer"]
