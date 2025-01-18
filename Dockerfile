# --- Stage 1: Build the Rust application ---
FROM rust:1.84 as builder

WORKDIR /app
COPY src src
COPY Cargo.toml Cargo.toml

# Install build-time dependencies for pcap & ffmpeg
RUN apt-get update && apt-get install -y --no-install-recommends \
    libpcap-dev

RUN cargo build --release

# --- Stage 2: Final runtime container ---
FROM debian:stable-slim

RUN apt-get update && apt-get install -y ffmpeg libpcap0.8 && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /app/hls

WORKDIR /app
COPY --from=builder /app/target/release/mpegts_to_s3 /app/mpeg_to_s3

ENTRYPOINT ["/app/mpeg_to_s3"]

