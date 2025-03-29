#!/bin/sh

cleanup() {
    echo "Caught signal, cleaning up..."
    if [ ! -z "$NODE_PID" ]; then
        echo "Killing agent process with PID: $NODE_PID"
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

if [ -z "${AGENT_SWAGGER_FILE}" ]; then
    export AGENT_SWAGGER_FILE=swagger_agent.yaml
    echo "Swagger file not set, using default of ${AGENT_SWAGGER_FILE}"
fi
if [ ! -f "${AGENT_SWAGGER_FILE}" ]; then
    echo "WARNING: Swagger file not found: ${AGENT_SWAGGER_FILE}"
    pwd
    ls -la $(pwd)
    exit 1
fi

if [ ! -d "node_modules" ]; then
    echo "Node modules not found, installing..."
    npm install
fi

## Start the agent (we are in hls/ directory one above the agent.js)
npm --prefix .. run start:agent &
NODE_PID=$!
echo "Started agent.js with PID: $NODE_PID"

wait $NODE_PID
