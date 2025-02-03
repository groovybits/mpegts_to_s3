#!/bin/sh

export RUST_BACKTRACE=1
export RUST_LOG=info

cleanup() {
    echo "Caught signal, cleaning up..."
    kilall hls-to-udp
    exit 0
}

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
            $@
    sleep 1
done