# MinIO MPEG-TS UDP Stream Uploader

This Rust application captures MPEG-TS UDP multicast streams, segments them, and uploads the segments to MinIO storage.

```bash
cargo build
mkdir hls
cd hls

# Capture udp://227.1.1.102:4102 from network interface net1 with segments in ./ts/ directory and upload to MinIO/S3
../target/debug/mpegts_to_s3 -i 227.1.1.102 -p 4102 -o ts -n net1
```

## Prerequisites

- Rust toolchain ([install here](https://rustup.rs/))
- MinIO server container [scripts/minio_server.sh](scripts/minio_server.sh)
- PCAP library (ensure `libpcap` is installed)
- FFmpeg installed for stream handling

## Configuration

1. Start your MinIO server locally:
   ```bash
   scripts/minio_server.sh
   ```

2. Update the application:
   - Change your MinIO endpoint (`http://127.0.0.1:9000`) via the command line args.
   - Adjust the bucket name and other configurations as needed via command line args.

3. Optional setup RAM disk `/mnt/ramdisk` using [scripts/ramdisk_manager.py](scripts/ramdisk_manager.py):
   ```bash
   scripts/ramdisk_manager.py -h
   ```

## Build and Run

1. Build the application:
   ```bash
   cargo build --release
   ```

2. Run the application with elevated privileges for pcap:
   ```bash
   sudo ./target/release/mpegts_to_s3 -i 227.1.1.102 -p 4102 -o ts -n net1
   ```

- Monitor uploads in MinIO's web interface or your S3-compatible client.
