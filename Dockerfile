FROM rust:slim-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./

Run mkdir src && \
echo "fn main() {}" > src/main.rs && \
cargo build --release --locked && \
rm -rf src

COPY src ./src

RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
apt-get install -y --no-install-recommends ca-certificates && \
rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 10001 appuser
USER appuser

WORKDIR /app

COPY --from=builder /app/target/release/mke_trash /usr/local/bin/app

ENV ADDRESS="200 E Wells St, Milwaukee, WI 53202"
ENV LAT="43.0417"
ENV LNG="-87.9098"

EXPOSE 8577

CMD ["app"]

