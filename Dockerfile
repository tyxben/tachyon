FROM rust:latest AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --bin tachyon-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/tachyon-server /usr/local/bin/tachyon-server
COPY --from=builder /build/config/default.toml /etc/tachyon/config.toml
EXPOSE 8080 8081
ENTRYPOINT ["tachyon-server", "/etc/tachyon/config.toml"]
