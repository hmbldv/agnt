# syntax=docker/dockerfile:1
FROM rust:1.95-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN cargo build --release --bin dmn

# ---- runtime ----
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 1000 -m dmn

COPY --from=builder /build/target/release/dmn /usr/local/bin/dmn

USER dmn
ENV DMN_CONFIG=/etc/dmn/dmn.toml
EXPOSE 7770

ENTRYPOINT ["/usr/local/bin/dmn"]
