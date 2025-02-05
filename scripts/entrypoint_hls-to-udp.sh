#!/bin/sh

export RUST_BACKTRACE=1
export RUST_LOG=info

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

trap cleanup SIGINT SIGTERM

while [ : ]; do
    hls-to-udp \
        -u ${HLS_INPUT_URL} \
        -o ${UDP_OUTPUT_IP}:${UDP_OUTPUT_PORT} \
        -l ${SMOOTHER_LATENCY} \
        -p ${M3U8_UPDATE_INTERVAL_MS} \
        -s ${HLS_HISTORY_SIZE} \
        -q ${SEGMENT_QUEUE_SIZE} \
        -z ${UDP_QUEUE_SIZE} \
        -b ${UDP_SEND_BUFFER} \
        ${SMOOTHER_ARGS} \
        ${EXTRA_ARGS} \
            $@
    sleep 1
done