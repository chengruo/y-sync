# y-sync 服务端（Rust）多阶段构建
FROM rust:1.83-slim AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY rust ./rust
RUN cargo build --release -p ysync-server-rs

FROM debian:bookworm-slim
RUN apt-get update -qq && apt-get install -y -qq ca-certificates && rm -rf /var/lib/apt/lists/*
RUN useradd --system --user-group --home-dir /var/lib/y-sync --shell /usr/sbin/nologin y-sync \
    && mkdir -p /var/lib/y-sync && chown y-sync:y-sync /var/lib/y-sync
COPY --from=builder /build/target/release/ysync-server-rs /usr/local/bin/y-sync-server-rs
USER y-sync
WORKDIR /var/lib/y-sync
EXPOSE 8720
ENTRYPOINT ["y-sync-server-rs", "serve", "-addr", "0.0.0.0:8720", "-data", "/var/lib/y-sync"]
