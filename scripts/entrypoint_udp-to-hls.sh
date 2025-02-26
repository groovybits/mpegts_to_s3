#!/bin/sh

cleanup() {
    echo "Caught signal, cleaning up..."
    killall udp-to-hls
    exit 0
}

if [ -f "${CONFIG_FILE}" ]; then
    . ${CONFIG_FILE}
    CONFIG_ARGS=". ${CONFIG_FILE}"
fi

if [ "${USE_UNSIGNED_URLS}" = "true" ]; then
    UNSIGNED_URL_ARGS="--unsigned_urls"
fi

if [ "${QUIET}" = "true" ]; then
    ARG_QUIET="--quiet"
fi

if [ "${DROP_CORRUPT_TS}" = "true" ]; then
    ARG_DROP_CORRUPT_TS="--drop-corrupt-ts"
fi

trap cleanup SIGINT SIGTERM

while [ : ]; do
    ${CONFIG_ARGS}
    RUST_BACKTRACE=full udp-to-hls \
        -n ${NETWORK_INTERFACE} \
        -i ${SOURCE_IP} \
        -p ${SOURCE_PORT} \
        -r ${MINIO_REGION_NAME} \
        -e ${MINIO_SERVER_URL} \
        -b ${MINIO_BUCKET_NAME} \
        -o ${CHANNEL_NAME} \
        --hls_keep_segments ${M3U8_LIVE_SEGMENT_COUNT} \
        ${UNSIGNED_URL_ARGS} ${ARG_QUIET} ${ARG_DROP_CORRUPT_TS} $@

    sleep 1
done
