#!/bin/sh

cleanup() {
    echo "Caught signal, cleaning up..."
    kilall udp-to-hls
    exit 0
}

if [ "${CAPTURE_TO_DISK}" = "true" ]; then
    CAPTURE_TO_DISK="--capture_to_disk"
else
    CAPTURE_TO_DISK=""
fi

if [ "${USE_UNSIGNED_URLS}" = "true" ]; then
    UNSIGNED_URL_ARGS="--unsigned_urls"
fi

if [ "${QUIET}" = "true" ]; then
    QUIET="-q"
fi

trap cleanup SIGINT SIGTERM

while [ : ]; do
    RUST_BACKTRACE=full udp-to-hls \
        -n ${NETWORK_INTERFACE} \
        -i ${SOURCE_IP} \
        -p ${SOURCE_PORT} \
        -e ${MINIO_SERVER_URL} \
        -b ${MINIO_BUCKET_NAME} \
        -o ${CHANNEL_NAME} \
        --hls_keep_segments ${M3U8_LIVE_SEGMENT_COUNT} \
        ${CAPTURE_TO_DISK} ${UNSIGNED_URL_ARGS} ${QUIET} $@

    sleep 1
done