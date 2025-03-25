#!/bin/sh

cleanup() {
    echo "Caught signal, cleaning up..."
    killall recording-playback-server
    exit 0
}

if [ -f "${CONFIG_FILE}" ]; then
    echo "Using config file: ${CONFIG_FILE}"
    CONFIG_ARGS=". ${CONFIG_FILE}"
    . ${CONFIG_FILE}
fi

if [ "${QUIET}" = "true" ]; then
    echo "Running in quiet mode"
fi

trap cleanup SIGINT SIGTERM

NODE_VERSION=$(node -v)
echo "Node version: ${NODE_VERSION}"
CWD=$(pwd)
echo "Current working directory: ${CWD}"

if [ -z "${SWAGGER_FILE}" ]; then
    export SWAGGER_FILE=swagger.yaml
    echo "Swagger file not set, using default of ${SWAGGER_FILE}"
fi
if [ ! -f "${SWAGGER_FILE}" ]; then
    echo "WARNING: Swagger file not found: ${SWAGGER_FILE}"
    pwd
    ls -la `pwd`
    exit 1
fi

if [ ! -d "node_modules" ]; then
    echo "Node modules not found, installing..."
    npm install
fi

while [ : ]; do
    ${CONFIG_ARGS} node ../server.js $@
    sleep 1
done