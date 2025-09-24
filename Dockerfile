FROM rust:1.82-slim-bookworm as builder

ENV DEBIAN_FRONTEND=noninteractive

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm -rf src

COPY src ./src
COPY example.config.toml ./
RUN touch src/main.rs
RUN cargo build --release

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 appuser

WORKDIR /app

COPY --from=builder --chown=appuser:appuser /app/target/release/azoth-balancer /usr/local/bin/

RUN mkdir -p /etc/azoth-balancer && chown appuser:appuser /etc/azoth-balancer

USER appuser

EXPOSE 8549

HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8549/health || exit 1

CMD ["azoth-balancer", "--config", "/etc/azoth-balancer/config.toml"]