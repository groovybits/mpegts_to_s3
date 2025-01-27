# Rust HLS to MpegTS UDP Client

This Rust-based client reads from an HLS M3U8 playlist and rebroadcasts it as MPEG-TS over UDP.

## Features

- Read from HLS M3U8 playlists
- Rebroadcast as MPEG-TS over UDP
- High performance and low latency

## Requirements

- Rust (latest stable version)
- Cargo (Rust package manager)

## Installation

2. Build the project:
    ```sh
    cargo build --release
    ```

## Usage

1. Run the client with the M3U8 URL and UDP address:
    ```sh
    ./target/release/hls-to-udp -u <M3U8_URL> -o <UDP_ADDRESS>
    ```

    Example:
    ```sh
    ./target/release/hls-to-udp -u http://example.com/playlist.m3u8 -o 239.0.0.1:1234 -p 100
    ```
