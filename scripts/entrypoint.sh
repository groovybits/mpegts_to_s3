#!/bin/sh

/app/mpegts_to_s3 \
    -n ${NETWORK_INTERFACE} \
    -i ${SOURCE_IP} \
    -p ${SOURCE_PORT} \
    -e ${MINIO_SERVER_URL} \
    -b ${MINIO_BUCKET_NAME} \
    -o ts \
    --encode 