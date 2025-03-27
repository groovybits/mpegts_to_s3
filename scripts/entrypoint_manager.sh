#!/bin/sh

cleanup() {
    echo "Caught signal, cleaning up..."
    if [ ! -z "$NODE_PID" ]; then
        echo "Killing manager process with PID: $NODE_PID"
        kill $NODE_PID
        wait $NODE_PID
    fi
    exit 0
}

trap cleanup SIGINT SIGTERM

if [ -f "${CONFIG_FILE}" ]; then
    echo "Using config file: ${CONFIG_FILE}"
    . ${CONFIG_FILE}
fi

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
    ls -la $(pwd)
    exit 1
fi

if [ ! -d "node_modules" ]; then
    echo "Node modules not found, installing..."
    npm install
fi

## Start the manager
npm run start:manager &
NODE_PID=$!
echo "Started manager.js with PID: $NODE_PID"

wait $NODE_PID
