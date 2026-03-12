# Stage 1: Build using the musl target
FROM rust:latest as builder

# 1. Install musl-tools and standard build-essential
RUN apt-get update && apt-get install -y \
    musl-tools \
    build-essential \
    && rm -rf /var/lib/apt/lists/*
# Install the musl target
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /app
COPY . .

# 3. Inform the 'cc' crate which linker to use
ENV CC=musl-gcc

# Build specifically for musl
RUN cargo build --release --target x86_64-unknown-linux-musl
RUN ls -l .
RUN ls -l target/x86_64-unknown-linux-musl/release
RUN ls -l target/x86_64-unknown-linux-musl/release/trmnl-rs

# Stage 2: The tiny Alpine runtime
FROM alpine:latest
WORKDIR /root

# Note the different path to the binary in the target folder
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/trmnl-rs .
COPY --from=builder /app/assets assets
COPY --from=builder /app/.env .env


CMD ["./trmnl-rs"]