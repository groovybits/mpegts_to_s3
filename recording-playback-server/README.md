# Recording and Playback API Server

This project includes an API server written in Node.js consisting of a Manager and Agent(s) that handle recording and playback jobs using udp-to-hls and hls-to-udp. The Manager/Agent Servers use Express and provides a comprehensive Swagger UI for API documentation.

## Features

1. **Manager Endpoints**  
	- Create, list, and manage recording and playback jobs.

2. **Agent Endpoints**  
	- Launch and stop recording/playback processes using udp-to-hls and hls-to-udp binaries. Child processes are spawned with their stdout and stderr streams monitored, ensuring logs are output and buffers remain drained.

3. **Swagger Documentation**  
	- Interactive API documentation is available at `/api-docs`.

4. **S3 Integration**  
	- Job metadata, including process IDs, are stored in an S3 bucket for tracking both active and completed jobs.

5. **Containerization**  
	- The server is containerized using for easy deployment.

## Setup and Running (Container Image without compose file, not recommended)

1. **Build the Images**
	You need podman and podman-compose installed or substitute with docker and docker-compose. (requires more manual w/out make, read Makefile for commands)

	From the project root, run:
	```bash
	make manager_image && \
	make agent_image
	```

2. **Run the Docker Container**  
	```bash
	make compose_build && make run
	```

3. **Access Swagger UI**
	Open your browser and navigate to [http://localhost:3000/api-docs](http://localhost:3000/api-docs) and [http://localhost:3001/api-docs](http://localhost:3001/api-docs) to view and test API endpoints.

## Setup and Running (Native Linux/Mac)

1. **Install Dependencies**  
	Ensure Node.js and npm are installed. From the project root, run:
	```bash
	npm install
	```
	Build the udp-to-hls and hls-to-udp binaries:
	```bash
	cd ../
	make setup # one time setup of system for Linux specifically
	make && make install # installs in ./bin/ directory
	cd -
	```

2. **Start the Server**  
	Set env variables (can store in `.env`) Replace 192.168.130.93 with your server IP.
	```bash
		SERVER_HOST=192.168.130.93
		SERVER_PORT=3000
		MINIO_ROOT_USER=minioadmin
		MINIO_ROOT_PASSWORD=minioadmin
		AWS_S3_ENDPOINT=http://192.168.130.93:9000
		URL_SIGNING_SECONDS=604800
		SEGMENT_DURATION_MS=2000
		MAX_SEGMENT_SIZE_BYTES=5000000
		USE_ESTIMATED_DURATION=false
	```
	Launch the API server with:
	```bash
	npm run start:manager && npm run start:agent # Must be root for pcap capture (Linux/Mac) permissions
	```
	The manager and agent listen on port 3000 and 3001 by default.

3. **Access Swagger UI**  
	Open your browser and navigate to [http://localhost:3000/api-docs](http://localhost:3000/api-docs) or [http://localhost:3001/api-docs](http://localhost:3001/api-docs) to view and test API endpoints.

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
  Job metadata (including process IDs) is stored in S3 for both manager and agent operations.

---

## Prerequisites

- **MinIO/S3 Server:** Ensure MinIO is available locally or via a container.
- **Dependencies:**  
  - Install libpcap for packet capture.
  - build ../bin/udp-to-hls and ../bin/hls-to-udp binaries for recording and playback.
- **Ports:**  
  - Open ports 3000, 3001 (API server) and 9000 (MinIO/S3).
- **Node.js:**  
  - Required for running the API server (`manager.js and agent.js`).

---
