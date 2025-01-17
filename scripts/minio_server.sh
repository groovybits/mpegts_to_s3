#!/bin/sh

S3_USERNAME=${S3_USERNAME:-minioadmin}
S3_PASSWORD=${S3_PASSWORD:-ThisIsSecret12345.}
S3_REGION=${S3_REGION:-us-east-1}
MINIO_DOMAIN=${MINIO_DOMAIN:-localhost}
MINIO_SERVER_URL=${MINIO_SERVER_URL:-http://127.0.0.1:9000}
MINIO_BROWSER_REDIRECT_URL=${MINIO_BROWSER_REDIRECT_URL:-http://127.0.0.1:9001}
DATA_DIR=${DATA_DIR:-$(pwd)/data}
CERTS_DIR=${CERTS_DIR:-$(pwd)/certs}

# SSH Tunnel to Minio Server through 127.0.0.1:9001 and 127.0.0.1:9000
# See scripts/minio_tunnel.sh
mkdir -p data
mkdir -p certs
chmod 777 data certs

podman stop s3minio 2>/dev/null
podman rm s3minio 2>/dev/null

podman run -p 127.0.0.1:9000:9000 -p 127.0.0.1:9001:9001 \
    --name s3minio \
    -e "MINIO_ROOT_USER=$S3_USERNAME" \
    -e "MINIO_ROOT_PASSWORD=$S3_PASSWORD" \
    --ulimit nofile=65535:65535 \
    -e MINIO_REGION_NAME="$S3_REGION" \
    -e "MINIO_DOMAIN=$MINIO_DOMAIN" \
    -e "MINIO_SERVER_URL=$MINIO_SERVER_URL" \
    -e "MINIO_BROWSER_REDIRECT_URL=$MINIO_BROWSER_REDIRECT_URL" \
    -v $DATA_DIR:/data:z \
    -v $CERTS_DIR:/root/.minio/certs:z \
    quay.io/minio/minio server /data --console-address ":9001" \
        --anonymous --json --quiet --certs-dir /root/.minio/certs $@
