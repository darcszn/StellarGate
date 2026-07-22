# ── Stage 1: dependency cache via cargo-chef ─────────────────────────────────
FROM rust:1.88-bookworm AS chef
RUN cargo install cargo-chef@0.1.68 --locked
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Build dependencies only — cached unless Cargo.toml/Cargo.lock change
# RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# ── Stage 2: slim runtime image ───────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -r -u 1001 -U stellargate \
    && mkdir /data \
    && chown stellargate:stellargate /data

COPY --from=builder /app/target/release/stellargate /usr/local/bin/stellargate

USER stellargate

ENV DATABASE_URL=sqlite:///data/stellargate.db

EXPOSE 3000

CMD ["/usr/local/bin/stellargate"]
