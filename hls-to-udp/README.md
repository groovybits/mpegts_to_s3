# HLS VOD to MPEG-TS UDP Re-cast

This Rust-based client reads from an HLS M3U8 playlist and rebroadcasts it as MPEG-TS over UDP. It is designed to handle the HLS produced by https://github.com/groovybits/mpegts_to_s3.git and is intended to be used as a relay for replaying that content.

To smooth the output using the LTN TS Tools Bitrate Smoother Rust Bindings [https://github.com/LTNGlobal-opensource/libltntstools](https://github.com/LTNGlobal-opensource/libltntstools) you must build with the `libltntstools_enabled` feature flag set.

## Features

- Read from HLS M3U8 playlists
- Rebroadcast as MPEG-TS over UDP

## Requirements

- Rust (latest stable version)
- Cargo (Rust package manager)
- LibPcap
- LibLTNTSTools (optional) Use the `libltntstools_enabled` feature flag to enable the Bitrate Smoother

## Installation

1. Build the project:
    ```sh
    cargo build --release
    ```

## Live vs. VOD Mode

- **Live Mode**: The client will read the M3U8 playlist and rebroadcast it as MPEG-TS over UDP.
- **VOD Mode**: The client will read the M3U8 playlist and rebroadcast it as MPEG-TS over UDP, but will only send the segments between the `start_time` and `end_time` parameters.

You need to set the `--vod` flag to enable VOD mode. The `--start-time` and `--end-time` flags are used to specify the start and end times in seconds.

### VOD Mode Example

This will playback the hour of 2025/02/05/05 between 10 and 120 seconds for that hour.

```sh
hls-to-udp -u "http://127.0.0.1:9000/hls/channel01/2025/02/05/05/index.m3u8/index.m3u8" \
    -o 224.0.0.200:4800 \
    --vod --start-time 10000 
    --end-time 120000
```

## Usage

    **hls-to-udp [OPTIONS] --m3u8-url <m3u8_url> --udp-output <udp_output_port:udp_output_ip>**

    | Option                                                 | Description                                       | Default |
    |--------------------------------------------------------|---------------------------------------------------|---------|
    | **-u, --m3u8-url <m3u8_url>**                          | The URL of the M3U8 playlist                      | (none)  |
    | **-o, --udp-output <udp_output>**                      | The destination UDP address                       | (none)  |
    | **-p, --poll-ms <poll_ms>**                            | The poll interval in milliseconds                 | 100     |
    | **-s, --history-size <history_size>**                  | The number of segments to retain                  | 999999  |
    | **-v, --verbose <verbose>**                            | Verbose mode                                      | 0       |
    | **-l, --latency <latency>**                            | Additional latency in milliseconds                | 2000    |
    | **-c, --pcr-pid <pcr_pid>**                            | PID to use for PCR timestamps                     | 0x00    |
    | **--smoother_buffers <count>**                         | Smoother buffer count                             | 5000    |
    | **-k, --packet-size <packet_size>**                    | TS packet size in bytes                           | 1316    |
    | **-q, --segment-queue-size <segment_queue_size>**      | The queue size for segments                       | 3       |
    | **-z, --udp-queue-size <udp_queue_size>**              | The queue size for UDP packets                    | 1       |
    | **--use-smoother**                                     | Use the LibLTNTSTools Bitrate Smoother            | false   |
    | **--udp-send-buffer <udp_send_buffer>**                | Size of the UDP Send buffer                       | 1358    |
    | **--vod**                                              | Use VOD mode                                      | false   |
    | **--start-time <start_time>**                          | Start time in seconds                             | 0       |
    | **--end-time <end_time>**                              | End time in seconds                               | 0       |
    | **-h, --help**                                         | Print help                                        | -       |
    | **-V, --version**                                      | Print version                                     | -       |

## Environment Variables

    - `HLS_INPUT_URL`: hls-to-udp input URL (default: `http://127.0.0.1:3001/channel01.m3u8`)
    - `UDP_OUTPUT_IP`: hls-to-udp output IP for UDP (default: `224.0.0.200`)
    - `UDP_OUTPUT_PORT`: hls-to-udp output port for UDP (default: `10000`)
    - `SMOOTHER_LATENCY`: Bitrate Smoother latency in milliseconds (default: `1000`)
    - `M3U8_UPDATE_INTERVAL_MS`: M3U8 update interval in milliseconds (default: `100`)
    - `HLS_HISTORY_SIZE`: HLS history size (default: `1800`)
    - `SEGMENT_QUEUE_SIZE`: Segment queue size (default: `32`)
    - `UDP_QUEUE_SIZE`: UDP queue size (default: `1024`)
    - `UDP_SEND_BUFFER`: Size of the UDP Send buffer (default: `0` - OS default)
    - `USE_SMOOTHER`: Use the LibLTNTSTools Bitrate Smoother (default: `false`) Requires --features=libltntstools_enabled

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
