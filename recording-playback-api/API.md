# API Reference

This document provides a simplified overview of the Record/Playback Manager and Agent API endpoints.

## Manager API (Port 3000)

The Manager API handles orchestration of recording and playback jobs, storage pools, and assets.

| Endpoint | Method | Description |
|----------|--------|-------------|
| **Recording Endpoints** |  |  |
| `/v1/recordings` | POST | Create a new recording job |
| `/v1/recordings` | GET | List all active or recent recordings |
| `/v1/recordings/{recordingId}` | GET | Get details of a specific recording |
| `/v1/recordings/{recordingId}` | DELETE | Cancel an existing recording job |
| **Playback Endpoints** |  |  |
| `/v1/playbacks` | POST | Create a new playback job |
| `/v1/playbacks` | GET | List all playback jobs |
| `/v1/playbacks/{playbackId}` | GET | Get detailed information about a playback |
| `/v1/playbacks/{playbackId}` | DELETE | Cancel a playback job |
| **Storage Pool Endpoints** |  |  |
| `/v1/pools` | POST | Create a new storage pool |
| `/v1/pools` | GET | List all storage pools |
| `/v1/pools/{poolId}/assets` | GET | List assets within a specific pool |
| `/v1/pools/{poolId}/assets` | DELETE | Delete an asset from a pool |
| **Asset Endpoints** |  |  |
| `/v1/assets/{assetId}` | GET | Get metadata for a single asset |
| **Admin Endpoints** |  |  |
| `/v1/admin/stats` | GET | Retrieve system-wide metrics and usage statistics |

## Agent API (Port 3001)

The Agent API handles the actual execution of recording and playback operations using the udp-to-hls and hls-to-udp binaries.

| Endpoint | Method | Description |
|----------|--------|-------------|
| **Recording Job Endpoints** |  |  |
| `/v1/agent/jobs/recordings` | POST | Start a new recording job on the agent |
| `/v1/agent/recordings/{jobId}` | DELETE | Stop a recording job on the agent |
| **Playback Job Endpoints** |  |  |
| `/v1/agent/jobs/playbacks` | POST | Start a new playback job on the agent |
| `/v1/agent/playbacks/{jobId}` | DELETE | Cancel a playback job on the agent |
| **Status Endpoints** |  |  |
| `/v1/agent/status` | GET | Get agent status and list active jobs |

## Authentication

All endpoints require Bearer token authentication. Include the JWT token in the Authorization header.

```
Authorization: Bearer <your_token>
```

## Resources

For full API documentation with request/response schemas, visit:
- Manager API Swagger UI: http://localhost:3000/api-docs
- Agent API Swagger UI: http://localhost:3001/api-docs