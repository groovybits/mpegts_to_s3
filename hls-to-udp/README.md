# HLS VOD to MPEG-TS UDP Re-cast

This Rust-based client reads from an HLS M3U8 playlist and rebroadcasts it as MPEG-TS over UDP. It is designed to handle the HLS produced by https://github.com/groovybits/mpegts_to_s3.git and is intended to be used as a relay for replaying that content.

To smooth the output we use the LTN TS Tools Bitrate Smoother [https://github.com/LTNGlobal-opensource/libltntstools/blob/master/src/bitrate_smoother.c](https://github.com/LTNGlobal-opensource/libltntstools/blob/master/src/bitrate_smoother.c). 

## Features

- Read from HLS M3U8 playlists
- Rebroadcast as MPEG-TS over UDP

## Requirements

- Rust (latest stable version)
- Cargo (Rust package manager)

## Installation

1. Build the project:
    ```sh
    cargo build --release
    ```

## Usage

1. Help output:
    ```sh
    HLS to UDP relay using LibLTNTSTools StreamModel and Smoother

    Usage: hls-to-udp [OPTIONS] --m3u8-url <m3u8_url> --udp-output <udp_output>

    Options:
    -u, --m3u8-url <m3u8_url>
    -o, --udp-output <udp_output>
    -p, --poll-ms <poll_ms>            [default: 100]
    -s, --history-size <history_size>  [default: 32]
    -v, --verbose <verbose>            [default: 0]
    -l, --latency <latency>            [default: 100]
    -c, --pcr-pid <pcr_pid>            [default: 0x00]
    -h, --help                         Print help
    -V, --version                      Print version
    ```

## Example

1. Run the client with the M3U8 URL and UDP address:
    ```sh
    ./target/release/hls-to-udp -u <M3U8_URL> -o <UDP_ADDRESS>
    ```

    Example:
    ```sh
    ./target/release/hls-to-udp -u http://example.com/playlist.m3u8 -o 224.0.0.200:10000
    ```

## Container Deployment

1. Build the container image:
    ```sh
    # The Dockerfile.replay and docker-compose.yaml are in the mpegts_to_s3 directory ../ below this one
    cd ../
    podman-compose up --build
    ```

## Usage outside of a container

1. Run the client with the M3U8 URL and UDP address:
    ```sh
    ./target/release/hls-to-udp -u <M3U8_URL> -o <UDP_ADDRESS>
    ```

    Example:
    ```sh
    ./target/release/hls-to-udp -u http://example.com/playlist.m3u8 -o 239.0.0.1:1234 -p 100
    ```

---

**Author:** wizard@groovy.org
**Date:** January 27, 2025
