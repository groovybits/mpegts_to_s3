#!/bin/sh

/app/hls-to-udp \
    -u ${HLS_INPUT_URL} \
    -o ${UDP_OUTPUT_IP}:${UDP_OUTPUT_PORT} \
         $@