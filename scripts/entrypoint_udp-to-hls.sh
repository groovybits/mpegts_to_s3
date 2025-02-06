#!/bin/sh

export RUST_BACKTRACE=1
export RUST_LOG=info

cleanup() {
    echo "Caught signal, cleaning up..."
    kilall udp-to-hls
    exit 0
}

if [ "${DISKLESS_MODE}" = "true" ]; then
    DISKLESS_ARGS="--diskless_mode"
else
    DISKLESS_ARGS=""
fi

trap cleanup SIGINT SIGTERM

while [ : ]; do
    udp-to-hls \
        -n ${NETWORK_INTERFACE} \
        -i ${SOURCE_IP} \
        -p ${SOURCE_PORT} \
        -e ${MINIO_SERVER_URL} \
        -b ${MINIO_BUCKET_NAME} \
        -o ${CHANNEL_NAME} \
        --hls_keep_segments ${M3U8_LIVE_SEGMENT_COUNT} \
        ${DISKLESS_ARGS} $@

    sleep 1
done