#!/bin/sh

export RUST_BACKTRACE=1
export RUST_LOG=info

cleanup() {
    echo "Caught signal, cleaning up..."
    kilall mpegts_to_s3
    exit 0
}

trap cleanup SIGINT SIGTERM

while [ : ]; do
    /app/mpegts_to_s3 \
        -n ${NETWORK_INTERFACE} \
        -i ${SOURCE_IP} \
        -p ${SOURCE_PORT} \
        -e ${MINIO_SERVER_URL} \
        -b ${MINIO_BUCKET_NAME} \
        -o ts \
        --manual_segment \
        --diskless_mode $@

    sleep 1
done