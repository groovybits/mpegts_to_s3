#!/bin/sh

cleanup() {
    echo "Caught signal, cleaning up..."
    kilall hls-to-udp
    exit 0
}

if [ "${USE_SMOOTHER}" = "true" ]; then
    SMOOTHER_ARGS="--use-smoother"
else
    SMOOTHER_ARGS=""
fi

if [ "${QUIET}" = "true" ]; then
    QUIET="--quiet"
fi

trap cleanup SIGINT SIGTERM

while [ : ]; do
    RUST_BACKTRACE=full hls-to-udp \
        -u ${HLS_INPUT_URL} \
        -o ${UDP_OUTPUT_IP}:${UDP_OUTPUT_PORT} \
        -l ${SMOOTHER_LATENCY} \
        -p ${M3U8_UPDATE_INTERVAL_MS} \
        -s ${HLS_HISTORY_SIZE} \
        -q ${SEGMENT_QUEUE_SIZE} \
        -z ${UDP_QUEUE_SIZE} \
        -b ${UDP_SEND_BUFFER} \
        -f "${HLS_TO_UDP_OUTPUT_FILE}" \
        -m ${MIN_UDP_PACKET_SIZE} \
        -k ${MAX_UDP_PACKET_SIZE} \
        ${SMOOTHER_ARGS} ${QUIET} \
        ${EXTRA_ARGS} \
            $@
    sleep 1
done