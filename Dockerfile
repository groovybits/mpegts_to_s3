# --- Stage 1: Build the Rust application ---
FROM rust:1.84 as builder

ARG DEBUG

WORKDIR /app
COPY src/main.rs src/main.rs
COPY Cargo.toml Cargo.toml

# Install build-time dependencies for pcap & ffmpeg
RUN apt-get update && apt-get install -y --no-install-recommends \
    libpcap-dev ffmpeg libavcodec-dev \
    libavfilter-dev libavformat-dev \
    libavutil-dev libpostproc-dev \
    libswresample-dev libswscale-dev \
    libavdevice-dev libclang-dev

RUN if [ "$DEBUG" = "true" ]; then \
        cargo build && \
            mkdir -p target/release && \
            cp -f target/debug/mpegts_to_s3 target/release/; \
    else \
        cargo build --release; \
    fi

# --- Stage 2: Final runtime container ---
FROM debian:stable-slim

RUN apt-get update && apt-get install -y ffmpeg libpcap0.8 && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /app/hls

WORKDIR /app/hls
COPY --from=builder /app/target/release/mpegts_to_s3 /app/mpegts_to_s3
COPY scripts/entrypoint.sh /app/entrypoint.sh

RUN /app/mpegts_to_s3 --version

ENTRYPOINT ["/app/entrypoint.sh"]

