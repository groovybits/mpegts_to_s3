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

    | hls-to-udp [OPTIONS] --m3u8-url <m3u8_url> --udp-output <udp_output_port:udp_output_ip>
    |
    | Option                               | Description                                 | Default  |
    |--------------------------------------|---------------------------------------------|----------|
    | **-u, --m3u8-url <m3u8_url>**        | The URL of the M3U8 playlist                | (none)   |
    | **-o, --udp-output <udp_output>**    | The destination UDP address                 | (none)   |
    | **-p, --poll-ms <poll_ms>**          | The poll interval in milliseconds           | 100      |
    | **-s, --history-size <history_size>**| The number of segments to retain            | 1800     |
    | **-v, --verbose <verbose>**          | Verbose mode                                | 0        |
    | **-l, --latency <latency>**          | Additional latency in milliseconds          | 100      |
    | **-c, --pcr-pid <pcr_pid>**          | PID to use for PCR timestamps               | 0x00     |
    | **-r, --rate <rate>**                | Bitrate in kbps                             | 5000     |
    | **-k, --packet-size <packet_size>**  | TS packet size in bytes                     | 1316     |
    | **-h, --help**                       | Print help                                  | -        |
    | **-V, --version**                    | Print version                               | -        |

## Environment Variables

    - `HLS_INPUT_URL`: hls-to-udp input URL (default: `http://127.0.0.1:3001/channel01.m3u8`)
    - `UDP_OUTPUT_IP`: hls-to-udp output IP for UDP (default: `224.0.0.200`)
    - `UDP_OUTPUT_PORT`: hls-to-udp output port for UDP (default: `10000`)

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
