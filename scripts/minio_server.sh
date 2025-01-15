#!/bin/sh

# SSH Tunnel to Minio Server through 127.0.0.1:9001
# See scripts/minio_tunnel.sh
mkdir -p data

podman stop s3minio 2>/dev/null
podman rm s3minio 2>/dev/null

podman run -p 127.0.0.1:9000:9000 -p 127.0.0.1:9001:9001 \
    --name s3minio \
    -e "MINIO_ROOT_USER=minioadmin" \
    -e "MINIO_ROOT_PASSWORD=minioadmin" \
    -v $(pwd)/data:/data \
    quay.io/minio/minio server /data --console-address ":9001" --anonymous
