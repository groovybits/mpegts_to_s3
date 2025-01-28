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
    /app/hls-to-udp \
        -u ${HLS_INPUT_URL} \
        -o ${UDP_OUTPUT_IP}:${UDP_OUTPUT_PORT} \
            $@
    sleep 1
done