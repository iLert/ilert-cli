FROM rust:1.96-bookworm AS builder
RUN apt-get update && apt-get install -y cmake
WORKDIR /usr/src/ilert
COPY . .
RUN cargo install --path .

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y openssl ca-certificates && rm -rf /var/lib/apt/lists/*
RUN update-ca-certificates
COPY --from=builder /usr/local/cargo/bin/ilert /usr/local/bin/ilert
ENTRYPOINT ["ilert"]