# Rust based Multicast MPEG-TS UDP Stream Pcap Capture for S3/MinIO HLS Hourly Archiving for Playback

This Rust application captures MPEG-TS UDP multicast streams as time based segments, creates an m3u8 playlist, uploads the segments to MinIO/S3 storage, signs the segments and a master playlist for playback.

```bash
# Capture udp://227.1.1.102:4102 from network interface net1 
# with segments in ./ts/ directory and upload to MinIO/S3
# using a server at http://192.168.130.93 port 3001 http, 9000 MinIO, 9001 MinIO Admin
git clone https://github.com/groovybits/mpegts_to_s3.git
cd mpegts_to_s3

# Build Rust mpegts_to_s3 program
cargo build --release

# Start MinIO Server on 127.0.0.1:9000 from a container w/podman
scripts/minio_server.py & # background, uses ./data/ for storage

# Create hls subdir for index.m3u8 serving
mkdir hls && cd hls 
# Run Python HTTP Server port 3001 from ./hls/ directory
../scripts/http_server.py &  # background, serves the current directory

# Run Rust mpegts_to_s3 collecting in ts/ directory from udp://227.1.1.102:4102
# as ts/year/month/day/hour/segment_YYYYMMDD-HHMMSS__0000.ts 10 second segments
SEGMENT_DURATION_SECONDS=10 \
  ../target/release/mpegts_to_s3 -i 227.1.1.102 -p 4102 \
    -o ts -n net1 --manual_segment --hls_keep_segments 3

# From another computer playback directly from the HTTP server
mpv -i http://192.168.130.93:3001/index.m3u8 

# Playback from minIO / S3 signed urls
## Get list of each hours signed master index URL
curl -s http://192.168.130.93:3001/ts/urls.log | tail -1 # Hour 2025/01/16/06 => http://...

## Setup SSH Tunnel into HTTP server
scripts/minio_admin.sh # ssh -p 3999 -L 9000:localhost:9000 -L 9001:localhost:9001 root@192.168.130.93 -N -f

## Playback the hourly_index.m3u8
mpv http://127.0.0.1:9000/ltnhls/2025/01/16/06/hourly_index.m3u8?x-id=GetObject&X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=minioadmin%2F20250116%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20250116T114743Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host&X-Amz-Signature=9271b9f8ff7ddffb4fa720b16077d5481926f7ad1d533474341502bc399e5fde
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
  -e, --endpoint <endpoint>
          S3-compatible endpoint [default: http://127.0.0.1:9000]
  -r, --region <region>
          S3 region [default: us-west-2]
  -b, --bucket <bucket>
          S3 bucket name [default: ltnhls]
  -i, --udp_ip <udp_ip>
          UDP multicast IP to filter [default: 227.1.1.102]
  -p, --udp_port <udp_port>
          UDP port to filter [default: 4102]
  -n, --interface <interface>
          Network interface for pcap [default: net1]
  -t, --timeout <timeout>
          Capture timeout in milliseconds [default: 1000]
  -o, --output_dir <output_dir>
          Local dir for HLS output (could be a RAM disk) [default: hls]
      --remove_local
          Remove local .ts/.m3u8 after uploading to S3?
      --manual_segment
          Perform manual TS segmentation + .m3u8 generation (no FFmpeg).
      --inject_pat_pmt
          If using manual segmentation, prepend the latest PAT & PMT to each segment.
      --hls_keep_segments <hls_keep_segments>
          Limit how many segments to keep in the index.m3u8 (0=unlimited). Also removes old .ts from disk. [default: 0]
  -h, --help
          Print help
  -V, --version
          Print version
```

- Monitor uploads in MinIO's web interface or your S3-compatible client.
