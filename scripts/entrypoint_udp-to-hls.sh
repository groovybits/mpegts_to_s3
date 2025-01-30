#!/bin/sh

export RUST_BACKTRACE=1
export RUST_LOG=info

cleanup() {
    echo "Caught signal, cleaning up..."
    kilall udp-to-hls
    exit 0
}

trap cleanup SIGINT SIGTERM

while [ : ]; do
    udp-to-hls \
        -n ${NETWORK_INTERFACE} \
        -i ${SOURCE_IP} \
        -p ${SOURCE_PORT} \
        -e ${MINIO_SERVER_URL} \
        -b ${MINIO_BUCKET_NAME} \
        -o ${CHANNEL_NAME} \
        --diskless_mode $@

    sleep 1
done