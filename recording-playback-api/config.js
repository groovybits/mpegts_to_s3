/*
 * Environment Variables:
 * - AGENT_ID: Unique identifier for this agent (default: agent-001)
 * - MANAGER_ID: Unique identifier for this manager (default: manager-001)
 * - SERVER_PROTOCOL: Protocol for the server (default: http)
 * - SERVER_PORT: Port for the server to listen on (default: 3000)
 * - SERVER_HOST: Host for the server to listen on (default: 127.0.0.1)
 * - AGENT_PROTOCOL: Protocol for the agent (default: http)
 * - AGENT_PORT: Port for the agent (default: 3001)
 * - AGENT_HOST: Host for the server to listen to on (default: 127.0.0.1)
 * - HLS_DIR: Directory for HLS files (default: empty)
 * - PLAYBACK_VERBOSE: Verbosity level for playback (default: 2)
 * - RECORDING_VERBOSE: Verbosity level for recording (default: 2)
 * - AGENT_SWAGGER_FILE: Path to the agent Swagger file (default: swagger_agent.yaml)
 * - MANAGER_SWAGGER_FILE: Path to the manager Swagger file (default: swagger_manager.yaml)
 * - HOURLY_URLS_LOG: Path to the hourly URLs log file (default: index.txt)
 * - RECIEVER_POLL_MS: Polling interval for the receiver (default: 10)
 * - SEGMENT_QUEUE_SIZE: Size of the segment queue (default: 1)
 * - UDP_QUEUE_SIZE: Size of the UDP queue (default: 1)
 * - AWS_S3_BUCKET: S3 bucket name (default: media)
 * - AWS_S3_ENDPOINT: Endpoint for the S3 server (default: http://127.0.0.1:9000)
 * - AWS_REGION: AWS region for S3 (default: us-east-1)
 * - AWS_ACCESS_KEY_ID: Access key for S3 (default: minioadmin)
 * - SMOOTHER_LATENCY: Smoother latency for hls-to-udp (default: 500)
 * - VERBOSE: Verbosity level for hls-to-udp (default: 2)
 * - UDP_BUFFER_BYTES: Buffer size for hls-to-udp (default: 0)
 * 
 * Config: {
 *   "serverVersion": "1.1.6",
 *   "AGENT_ID": "agent-001",
 *   "MANAGER_ID": "manager001",
 *   "SERVER_PROTOCOL": "http",
 *   "SERVER_PORT": "3000",
 *   "SERVER_HOST": "192.168.130.93",
 *   "AGENT_PROTOCOL": "http",
 *   "AGENT_PORT": "3001",
 *   "AGENT_HOST": "192.168.130.93",
 *   "serverUrl": "http://192.168.130.93:3000",
 *   "agentUrl": "http://192.168.130.93:3001",
 *   "s3endPoint": "http://192.168.130.93:9000",
 *   "s3Region": "us-east-1",
 *   "s3AccessKeyDB": "minioadmin",
 *   "s3SecretKeyDB": "minioadmin",
 *   "s3BucketDB": "media",
 *   "PLAYBACK_VERBOSE": 2,
 *   "RECORDING_VERBOSE": 2,
 *   "SMOOTHER_LATENCY": 100,
 *   "UDP_BUFFER_BYTES": 0,
 *   "AGENT_SWAGGER_FILE": "swagger_agent.yaml",
 *   "MANAGER_SWAGGER_FILE": "/app/swagger_manager.yaml",
 *   "ORIGINAL_DIR": "/app/",
 *   "HLS_DIR": "",
 *   "RECIEVER_POLL_MS": 10,
 *   "SEGMENT_QUEUE_SIZE": 1,
 *   "UDP_QUEUE_SIZE": 1,
 *   "HOURLY_URLS_INDEX": "/app/hls/index.txt",
 *   "CAPTURE_BUFFER_SIZE": "4193904",
 *   "SEGMENT_DURATION_MS": "2000",
 *   "URL_SIGNING_SECONDS": "604800",
 *   "USE_ESTIMATED_DURATION": "true",
 *   "MAX_SEGMENT_SIZE_BYTES": "5242756"
 * }
 * 
 * Usage:
 * - Start the server with `node server.js`
 * - Access the Swagger UI at http://localhost:3000/api-docs
 * - Use the API to create recordings and playbacks
 * 
 * URL parameters for UDP MpegTS:
 * - udp://multicast_ip:port?interface=net1
 * 
 * Dependencies:
 * - express: Web server framework
 * - @aws-sdk/client-s3: AWS SDK for S3
 * - udp-to-hls: UDP to HLS converter
 * - hls-to-udp: HLS to UDP converter
 * - MinIO: S3-compatible server
 * - node-fetch: Fetch API for Node.js
 * 
 * Build Instructions:
 * - Install Node.js and npm version greater than 18 ideally
 * - Run `npm install` to install dependencies
 * - Build the udp-to-hls and hls-to-udp in ../ one directory down using `make && make install`
 * - Run `npm run start:manager && npm run start:agent` to start the API Manager and Agent
 */
import fs from 'fs';
import packageJson from './package.json' with { type: 'json' };

const config = {
  serverVersion: packageJson.version,
  // Agent ID
  AGENT_ID: process.env.AGENT_ID || 'agent-001',
  MANAGER_ID: process.env.MANAGER_ID || 'manager-001',

  // Server Manager and Agent URL Bases used for fetch calls
  SERVER_PROTOCOL: process.env.SERVER_PROTOCOL || 'http',
  SERVER_PORT: process.env.SERVER_PORT || 3000,
  SERVER_HOST: process.env.SERVER_HOST || '127.0.0.1',

  AGENT_PROTOCOL: process.env.AGENT_PROTOCOL || 'http',
  AGENT_PORT: process.env.AGENT_PORT || 3001,
  AGENT_HOST: process.env.AGENT_HOST || '127.0.0.1',

  // Build the serverUrl using the above values
  serverUrl:
    (process.env.SERVER_PROTOCOL || 'http') +
    '://' +
    (process.env.SERVER_HOST || '127.0.0.1') +
    ':' +
    (process.env.SERVER_PORT || 3000),

  agentUrl:
    (process.env.AGENT_PROTOCOL || 'http') +
    '://' +
    (process.env.AGENT_HOST || '127.0.0.1') +
    ':' +
    (process.env.AGENT_PORT || 3001),

  // S3 endpoint for the MinIO server
  s3endPoint: process.env.AWS_S3_ENDPOINT || 'http://127.0.0.1:9000',
  s3Region: process.env.AWS_REGION || 'us-east-1',
  s3AccessKeyDB: process.env.AWS_ACCESS_KEY_ID || 'minioadmin',
  s3SecretKeyDB: process.env.AWS_SECRET_ACCESS_KEY || 'minioadmin',
  s3BucketDB: process.env.AWS_S3_BUCKET || 'media',

  // Runtime verbosity levels of Rust programs, 0-4: error, warn, info, debug, trace
  PLAYBACK_VERBOSE: process.env.PLAYBACK_VERBOSE || 2,
  RECORDING_VERBOSE: process.env.RECORDING_VERBOSE || 2,

  // Other configuration values
  SMOOTHER_LATENCY: process.env.SMOOTHER_LATENCY || 100,
  UDP_BUFFER_BYTES: process.env.UDP_BUFFER_BYTES || 0,
  AGENT_SWAGGER_FILE: process.env.AGENT_SWAGGER_FILE || 'swagger_agent.yaml',
  MANAGER_SWAGGER_FILE: process.env.MANAGER_SWAGGER_FILE || 'swagger_manager.yaml',
  ORIGINAL_DIR: process.cwd() + '/',
  HLS_DIR: process.env.HLS_DIR || '',
  RECIEVER_POLL_MS: process.env.RECIEVER_POLL_MS || 10,
  SEGMENT_QUEUE_SIZE: process.env.SEGMENT_QUEUE_SIZE || 1,
  UDP_QUEUE_SIZE: process.env.UDP_QUEUE_SIZE || 1,

  HOURLY_URLS_INDEX: process.env.HOURLY_URLS_LOG || process.cwd() + '/index.txt',

  CAPTURE_BUFFER_SIZE: process.env.CAPTURE_BUFFER_SIZE || `4193904`,
  SEGMENT_DURATION_MS: process.env.SEGMENT_DURATION_MS || `2000`,
  URL_SIGNING_SECONDS: process.env.URL_SIGNING_SECONDS || `604800`,
  USE_ESTIMATED_DURATION: process.env.USE_ESTIMATED_DURATION || `true`,
  MAX_SEGMENT_SIZE_BYTES: process.env.MAX_SEGMENT_SIZE_BYTES || `5242756`,
};

// Optionally, update the PATH if a bin directory exists one level up.
if (fs.existsSync('../bin/')) {
  process.env.PATH = process.env.PATH + ':../bin';
}

export default config;
