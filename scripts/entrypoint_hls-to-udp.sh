#!/bin/sh

cleanup() {
    echo "Caught signal, cleaning up..."
    killall hls-to-udp
    exit 0
}

if [ -f "${CONFIG_FILE}" ]; then
    echo "Using config file: ${CONFIG_FILE}"
    CONFIG_ARGS=". ${CONFIG_FILE}"
    . ${CONFIG_FILE}
fi

if [ "${USE_SMOOTHER}" = "true" ]; then
    echo "Using smoother"
    SMOOTHER_ARGS="--use-smoother"
else
    SMOOTHER_ARGS=""
fi

if [ "${QUIET}" = "true" ]; then
    echo "Running in quiet mode"
    ARG_QUIET="--quiet"
fi

if [ "${DROP_CORRUPT_TS}" = "true" ]; then
    echo "Dropping corrupt TS packets"
    ARG_DROP_CORRUPT_TS="--drop-corrupt-ts"
fi

trap cleanup SIGINT SIGTERM

while [ : ]; do
    ${CONFIG_ARGS}
    RUST_BACKTRACE=full hls-to-udp \
        -u ${HLS_INPUT_URL} \
        -o ${UDP_OUTPUT_IP}:${UDP_OUTPUT_PORT} \
        -l ${SMOOTHER_LATENCY} \
        -p ${M3U8_UPDATE_INTERVAL_MS} \
        --history-size ${HLS_HISTORY_SIZE} \
        -q ${SEGMENT_QUEUE_SIZE} \
        -z ${UDP_QUEUE_SIZE} \
        -b ${UDP_SEND_BUFFER} \
        -f "${HLS_TO_UDP_OUTPUT_FILE}" \
        -m ${MIN_UDP_PACKET_SIZE} \
        -k ${MAX_UDP_PACKET_SIZE} \
        ${SMOOTHER_ARGS} ${ARG_QUIET} ${ARG_DROP_CORRUPT_TS} $@
    sleep 1
done
