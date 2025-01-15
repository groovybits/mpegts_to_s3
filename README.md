# MinIO MPEG-TS UDP Stream Uploader

This Rust application captures MPEG-TS UDP multicast streams, segments them, and uploads the segments to MinIO storage.

```bash
# Capture udp://227.1.1.102:4102 from network interface net1 
# with segments in ./ts/ directory and upload to MinIO/S3
git clone https://github.com/groovybits/mpegts_to_s3.git
cd mpegts_to_s3

# Start MinIO Server on 127.0.0.1:9000 from a container w/podman
scripts/minio_server.py & # background

# Create hls subdir for index.m3u8 serving
mkdir hls && cd hls 
# Run Python HTTP Server port 3001
scripts/http_server.py &  # background

# Build Rust mpegts_to_s3 program
cargo build --release
# Run Rust mpegts_to_s3 collecting in ts/ directory
# as year/month/day/hour/segment{data}.ts 2 second segments
../target/release/mpegts_to_s3 -i 227.1.1.102 -p 4102 \
    -o ts -n net1 --manual_segment --inject_pat_pmt

# From another computer playback
mpv -i http://192.168.130.93:3001/index.m3u8 
```

## Prerequisites

- Newest Rust toolchain ([install here](https://rustup.rs/))
- MinIO server container or S3 [scripts/minio_server.sh](scripts/minio_server.sh)
- PCAP library (ensure `libpcap` is installed)
- FFmpeg installed for stream handling (optional)
- Ports 9000 and 9001 open for MinIO and HTTP server
- SSH Forwarding into 127.0.0.1:9001 on the host for HTTP server

## Configuration

```bash
PCAP capture -> HLS -> Directory Watch -> S3 Upload

Usage: mpegts_to_s3 [OPTIONS]

Options:
  -e, --endpoint <endpoint>      S3-compatible endpoint [default: http://127.0.0.1:9000]
  -r, --region <region>          S3 region [default: us-west-2]
  -b, --bucket <bucket>          S3 bucket name [default: ltnhls]
  -i, --udp_ip <udp_ip>          UDP multicast IP to filter [default: 227.1.1.102]
  -p, --udp_port <udp_port>      UDP port to filter [default: 4102]
  -n, --interface <interface>    Network interface for pcap [default: net1]
  -t, --timeout <timeout>        Capture timeout in milliseconds [default: 1000]
  -o, --output_dir <output_dir>  Local dir for HLS output (could be a RAM disk) [default: hls]
      --remove_local             Remove local .ts/.m3u8 after uploading to S3?
      --manual_segment           Perform manual TS segmentation + .m3u8 generation (no FFmpeg).
      --inject_pat_pmt           If using manual segmentation, prepend the latest PAT & PMT to each segment.
  -h, --help                     Print help
  -V, --version                  Print version
```

- Monitor uploads in MinIO's web interface or your S3-compatible client.
