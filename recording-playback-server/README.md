# Recorder and Playback API Server & Process Management (server.js)

This project includes an API server written in Node.js that manages recording and playback jobs. It uses Express and provides a comprehensive Swagger UI for API documentation.

## Features

1. **Manager Endpoints**  
	- Create, list, and manage recording and playback jobs.

2. **Agent Endpoints**  
	- Launch and stop recording/playback processes using udp-to-hls and hls-to-udp binaries. Child processes are spawned with their stdout and stderr streams monitored, ensuring logs are output and buffers remain drained.

3. **Swagger Documentation**  
	- Interactive API documentation is available at `/api-docs`.

4. **SQLite Integration**  
	- Job metadata, including process IDs, is stored in `media_jobs.db` for tracking both active and completed jobs.

## Setup and Running

1. **Install Dependencies**  
	Ensure Node.js and npm are installed. From the project root, run:
	```bash
	npm install
	```

2. **Start the Server**  
	Set env variables (can store in `.env`)
	```bash
	export SERVER_HOST=localhost
	export SERVER_PORT=3000
	export MINIO_ROOT_USER=minioadmin (S3 Access Key)
	export MINIO_ROOT_PASSWORD=minioadmin (S3 Secret Key)
	export AWS_S3_ENDPOINT=http://127.0.01:9000
	export URL_SIGNING_SECONDS=604800
	export SEGMENT_DURATION_MS=2000
	export MAX_SEGMENT_SIZE_BYTES=5000000
	```
	Launch the API server with:
	```bash
	node server.js
	```
	The server listens on port 3000 by default.

3. **Access Swagger UI**  
	Open your browser and navigate to [http://localhost:3000/api-docs](http://localhost:3000/api-docs) to view and test API endpoints.

## API Endpoints Overview

### Recordings
- **POST** `/v1/recordings` – Create a new recording job (manager forwards to agent).
- **GET** `/v1/recordings` – List all active or recent recordings.
- **GET** `/v1/recordings/:recordingId` – Retrieve details for a specific recording.
- **DELETE** `/v1/recordings/:recordingId` – Cancel a recording job.

### Pools & Assets
- **POST** `/v1/pools` – Create a new storage pool for S3/MinIO.
- **GET** `/v1/pools` – List all storage pools.
- **GET** `/v1/pools/:poolId/assets` – List assets within a specific pool.
- **DELETE** `/v1/pools/:poolId/assets` – Delete an asset from a pool.
- **GET** `/v1/assets/:assetId` – Get metadata for a single asset.

### Playbacks
- **POST** `/v1/playbacks` – Create a new playback job.
- **GET** `/v1/playbacks` – List all playback jobs.
- **GET** `/v1/playbacks/:playbackId` – Get details about a specific playback job.
- **DELETE** `/v1/playbacks/:playbackId` – Cancel a playback job.

### Agent Endpoints
- **POST** `/v1/agent/jobs/recordings` – Start a recording job on the agent.
- **DELETE** `/v1/agent/recordings/:jobId` – Stop a recording job on the agent.
- **POST** `/v1/agent/jobs/playbacks` – Start a playback job on the agent.
- **DELETE** `/v1/agent/playbacks/:jobId` – Stop a playback job on the agent.
- **GET** `/v1/agent/status` – Get agent status and list active jobs.

## Process Management

- **Child Process Handling:**  
  The server uses Node.js’s `spawn` to launch external processes. Both udp-to-hls and hls-to-udp are spawned with piped `stdout` and `stderr` streams to ensure output is logged and buffers do not block execution.

- **Automatic Job Termination:**  
  Jobs with a specified duration are automatically stopped using timeouts, with updates recorded in the SQLite database.

- **Job Tracking:**  
  Job metadata (including process IDs) is stored in `media_jobs.db` for both manager and agent operations.

---

## Prerequisites

- **MinIO/S3 Server:** Ensure MinIO is available locally or via a container.
- **Dependencies:**  
  - Install libpcap for packet capture.
  - FFmpeg (optional) for HLS segmentation.
  - udp-to-hls and hls-to-udp binaries for recording and playback.
- **Ports:**  
  - Open ports 3000 (API server) and 9000 (MinIO/S3).
- **Node.js:**  
  - Required for running the API server (`server.js`).

---
