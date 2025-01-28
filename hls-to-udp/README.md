# HLS VOD to MPEG-TS UDP Re-cast

This Rust-based client reads from an HLS M3U8 playlist and rebroadcasts it as MPEG-TS over UDP. It is designed to handle the HLS produced by https://github.com/groovybits/mpegts_to_s3.git and is intended to be used as a relay for replaying that content.

Currently it is recommended to smooth the output with the LTN TS Tools Bitrate Smoother [https://github.com/groovybits/ltntstools/blob/master/src/bitrate_smoother.c](https://github.com/groovybits/ltntstools/blob/master/src/bitrate_smoother.c). See the build project for that here [https://github.com/LTNGlobal-opensource/ltntstools-build-environment](https://github.com/LTNGlobal-opensource/ltntstools-build-environment).

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
