/****************************************************
 * server.js — Recording/Playback API Server
 ****************************************************/

const express = require('express');
const { v4: uuidv4 } = require('uuid');
const sqlite3 = require('sqlite3').verbose();
const { spawn } = require('child_process');

const fs = require('fs');

// For Swagger
const swaggerUi = require('swagger-ui-express');
const yaml = require('js-yaml');

// ----------------------------------------------------
// Embedded Swagger YAML as a multiline string
// (Or store in a .yaml or inline this.)
// ----------------------------------------------------
const swaggerYaml = `
openapi: 3.0.3
info:
  title: Recording & Playback 2025 API
  version: 1.0.0
  description: >
    Comprehensive API specification for the Recording/Playback stack.  
    This document covers:
    - **Manager API** (externally facing, single instance per cluster).  
    - **Agent API** (one instance per server, controlled by the Manager).  

    Key points from the specs:
    - **MPEG-TS** SPTS or MPTS as the storage/transfer format.
    - **Immediate start** for all recordings and playbacks (no future scheduling).
    - **S3-based Pools** for asset storage (AWS or MinIO), with flexible credentials.
    - **Durations** range from 10 seconds (minimum) to 6 hours (maximum).
    - **Bitrates** range from 1 Kb/s to 240 Mb/s.
    - Thumbnails generated approximately every 60 seconds (where applicable).
    - Full usage metrics and “god mode” under Admin endpoints.

servers:
  - url: http://localhost:3000/v1

tags:
  - name: Manager-Recordings
    description: Manager-facing endpoints to create and manage recording jobs.
  - name: Manager-Playbacks
    description: Manager-facing endpoints for playback control.
  - name: Manager-Pools
    description: Manage or query user-defined storage pools.
  - name: Manager-Assets
    description: Endpoints to query or delete stored assets.
  - name: Manager-Admin
    description: Administrative (support/god) mode operations.
  - name: Agent-Recordings
    description: Agent's recording execution endpoints.
  - name: Agent-Playbacks
    description: Agent's playback execution endpoints.
  - name: Agent-Misc
    description: Agent status, metrics, and other endpoints.

components:
  securitySchemes:
    BearerAuth:
      type: http
      scheme: bearer
      bearerFormat: JWT

security:
  - BearerAuth: []

paths:
  ########################################################
  # Manager API - RECORDINGS
  ########################################################

  /recordings:
    post:
      tags:
        - Manager-Recordings
      summary: Create a new recording
      description: >
        Initiates a new recording immediately, creating a growing asset.  
        According to specs:
        - Minimum duration: 10 seconds  
        - Maximum duration: 6 hours  
        - Accepts an LTN endpoint, a UDP URL, or an SRT URL.  
        - destinationProfile references internal AWS/MinIO credentials.
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              properties:
                sourceUrl:
                  type: string
                  description: LTN / UDP / SRT endpoint
                duration:
                  type: integer
                  description: Duration in seconds (10 sec to 6 hrs)
                destinationProfile:
                  type: string
                  description: Shortcut to internal credential set
              required:
                - sourceUrl
                - duration
                - destinationProfile
      responses:
        "201":
          description: Recording job created
          content:
            application/json:
              schema:
                type: object
                properties:
                  recordingId:
                    type: string
                    description: Unique identifier for the recording

    get:
      tags:
        - Manager-Recordings
      summary: List all active or recent recordings
      description: >
        Returns metadata about ongoing or recently completed recording jobs
        for the authenticated user.
      responses:
        "200":
          description: List of recordings
          content:
            application/json:
              schema:
                type: array
                items:
                  type: object
                  properties:
                    recordingId:
                      type: string
                    sourceUrl:
                      type: string
                    duration:
                      type: integer
                    destinationProfile:
                      type: string
                    startTime:
                      type: string
                      format: date-time
                    endTime:
                      type: string
                      format: date-time
                    status:
                      type: string
                      enum: [starting, active, completed, canceled]

  /recordings/{recordingId}:
    get:
      tags:
        - Manager-Recordings
      summary: Get details of a specific recording
      description: >
        Returns detailed status and metadata for a single recording job, including
        current size, bitrate, and thumbnail URL updated every 60 seconds.
      parameters:
        - in: path
          name: recordingId
          schema:
            type: string
          required: true
      responses:
        "200":
          description: Recording details
          content:
            application/json:
              schema:
                type: object
                properties:
                  recordingId:
                    type: string
                  sourceUrl:
                    type: string
                  duration:
                    type: integer
                  destinationProfile:
                    type: string
                  currentSize:
                    type: integer
                  startTime:
                    type: string
                    format: date-time
                  endTime:
                    type: string
                    format: date-time
                  bitrate:
                    type: integer
                  ccErrorCount:
                    type: integer
                  thumbnailUrl:
                    type: string
                  status:
                    type: string

    delete:
      tags:
        - Manager-Recordings
      summary: Cancel a recording
      parameters:
        - in: path
          name: recordingId
          schema:
            type: string
          required: true
      responses:
        "204":
          description: Recording canceled

  ########################################################
  # Manager API - POOLS
  ########################################################

  /pools:
    post:
      tags:
        - Manager-Pools
      summary: Create a new storage pool
      description: >
        Defines a user-specific S3 (or similar) bucket with credentials.
        Pools are not shared between users (yet).
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              properties:
                bucketName:
                  type: string
                  description: Name of the S3/MinIO bucket
                credentials:
                  type: object
                  properties:
                    accessKey:
                      type: string
                    secretKey:
                      type: string
                  required:
                    - accessKey
                    - secretKey
              required:
                - bucketName
                - credentials
      responses:
        "201":
          description: Pool created
          content:
            application/json:
              schema:
                type: object
                properties:
                  poolId:
                    type: string
                    description: Unique pool identifier
                  bucketName:
                    type: string

    get:
      tags:
        - Manager-Pools
      summary: Enumerate all pools for the authenticated user
      responses:
        "200":
          description: List of pools
          content:
            application/json:
              schema:
                type: array
                items:
                  type: object
                  properties:
                    poolId:
                      type: string
                    bucketName:
                      type: string

  /pools/{poolId}/assets:
    get:
      tags:
        - Manager-Assets
      summary: List assets within a specific pool
      parameters:
        - in: path
          name: poolId
          required: true
          schema:
            type: string
      responses:
        "200":
          description: Assets in the pool
          content:
            application/json:
              schema:
                type: array
                items:
                  type: object
                  properties:
                    assetId:
                      type: string
                      description: Globally unique (POOLID-ASSETID)
                    metadata:
                      type: object
                      description: Arbitrary metadata about the asset

    delete:
      tags:
        - Manager-Assets
      summary: Delete an asset from a pool
      parameters:
        - in: path
          name: poolId
          required: true
          schema:
            type: string
        - in: query
          name: assetId
          required: true
          schema:
            type: string
      responses:
        "204":
          description: Asset deleted

  ########################################################
  # Manager API - ASSETS
  ########################################################

  /assets/{assetId}:
    get:
      tags:
        - Manager-Assets
      summary: Get metadata for a single asset
      parameters:
        - in: path
          name: assetId
          required: true
          schema:
            type: string
      responses:
        "200":
          description: Asset metadata
          content:
            application/json:
              schema:
                type: object
                properties:
                  assetId:
                    type: string
                  poolId:
                    type: string
                  metadata:
                    type: object
                    description: Additional details (bitrate, size, etc.)

  ########################################################
  # Manager API - PLAYBACKS
  ########################################################

  /playbacks:
    post:
      tags:
        - Manager-Playbacks
      summary: Create a new playback job
      description: >
        Begins a playback job immediately (no scheduling).  
        Can handle static or growing assets for partial time range playback.  
        Accepts LTN, UDP, or SRT as destinations, plus S3 references.
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              properties:
                sourceProfile:
                  type: string
                  description: References credentials for reading the asset
                destinationUrl:
                  type: string
                  description: LTN, UDP, or SRT URL for playout
                duration:
                  type: integer
                  description: Duration in seconds (could be 'infinite' if 0 or omitted)
              required:
                - sourceProfile
                - destinationUrl
                - duration
      responses:
        "201":
          description: Playback job created
          content:
            application/json:
              schema:
                type: object
                properties:
                  playbackId:
                    type: string

    get:
      tags:
        - Manager-Playbacks
      summary: List all playback jobs
      responses:
        "200":
          description: A list of playback jobs for the user
          content:
            application/json:
              schema:
                type: array
                items:
                  type: object
                  properties:
                    playbackId:
                      type: string
                    sourceProfile:
                      type: string
                    destinationUrl:
                      type: string
                    duration:
                      type: integer
                    status:
                      type: string

  /playbacks/{playbackId}:
    get:
      tags:
        - Manager-Playbacks
      summary: Get details about a playback job
      parameters:
        - in: path
          name: playbackId
          required: true
          schema:
            type: string
      responses:
        "200":
          description: Playback details
          content:
            application/json:
              schema:
                type: object
                properties:
                  playbackId:
                    type: string
                  sourceProfile:
                    type: string
                  destinationUrl:
                    type: string
                  duration:
                    type: integer
                  currentSize:
                    type: integer
                  startTime:
                    type: string
                    format: date-time
                  endTime:
                    type: string
                    format: date-time
                  bitrate:
                    type: integer
                  ccErrorCount:
                    type: integer
                  thumbnailUrl:
                    type: string
                  status:
                    type: string

    delete:
      tags:
        - Manager-Playbacks
      summary: Cancel a playback job
      parameters:
        - in: path
          name: playbackId
          required: true
          schema:
            type: string
      responses:
        "204":
          description: Playback job canceled

  ########################################################
  # Manager API - ADMIN (SUPPORT / GOD MODE)
  ########################################################

  /admin/stats:
    get:
      tags:
        - Manager-Admin
      summary: System-wide metrics and usage
      description: >
        Provides consolidated stats such as concurrent usage, total assets, 
        cluster performance metrics, and system load.
      responses:
        "200":
          description: Admin statistics
          content:
            application/json:
              schema:
                type: object
                properties:
                  concurrentRecordings:
                    type: integer
                  concurrentPlaybacks:
                    type: integer
                  clusterUsage:
                    type: object
                  systemLoad:
                    type: number
                  totalAssets:
                    type: integer

  ########################################################
  # AGENT API - RECORDINGS
  ########################################################

  /agent/jobs/recordings:
    post:
      tags:
        - Agent-Recordings
      summary: Start a new recording on this Agent
      description: >
        The Manager instructs the Agent to launch a recording process.
        The Agent is responsible for spawning binaries, handling containerization, etc.
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              properties:
                jobId:
                  type: string
                sourceUrl:
                  type: string
                duration:
                  type: integer
                destinationProfile:
                  type: string
              required:
                - jobId
                - sourceUrl
                - duration
                - destinationProfile
      responses:
        "201":
          description: Agent accepted the recording job

  /agent/recordings/{jobId}:
    delete:
      tags:
        - Agent-Recordings
      summary: Stop a recording job on this Agent
      parameters:
        - in: path
          name: jobId
          required: true
          schema:
            type: string
      responses:
        "204":
          description: Recording job stopped

  ########################################################
  # AGENT API - PLAYBACKS
  ########################################################

  /agent/jobs/playbacks:
    post:
      tags:
        - Agent-Playbacks
      summary: Start a new playback job on this Agent
      description: >
        Manager instructs the Agent to commence a playback process,
        reading from a specified asset and streaming out via destinationUrl.
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              properties:
                jobId:
                  type: string
                sourceProfile:
                  type: string
                destinationUrl:
                  type: string
                duration:
                  type: integer
                vodStartTime:
                  type: string
                  description: "Optional. E.g. '2025/03/04 08:10:00'"
                vodEndTime:
                  type: string
                  description: "Optional. E.g. '2025/03/04 08:20:00'"
              required:
                - jobId
                - sourceProfile
                - destinationUrl
                - duration
      responses:
        "201":
          description: Playback job accepted

  /agent/playbacks/{jobId}:
    delete:
      tags:
        - Agent-Playbacks
      summary: Cancel a playback job on this Agent
      parameters:
        - in: path
          name: jobId
          required: true
          schema:
            type: string
      responses:
        "204":
          description: Playback job canceled

  ########################################################
  # AGENT API - MISC
  ########################################################

  /agent/status:
    get:
      tags:
        - Agent-Misc
      summary: Check Agent status and currently running jobs
      description: >
        Returns health indicators and lists active recording/playback jobs
        on this specific Agent node.
      responses:
        "200":
          description: Agent status returned
          content:
            application/json:
              schema:
                type: object
                properties:
                  agentId:
                    type: string
                  status:
                    type: string
                  activeRecordings:
                    type: array
                    items:
                      type: object
                      properties:
                        jobId:
                          type: string
                        sourceUrl:
                          type: string
                        duration:
                          type: integer
                        destinationProfile:
                          type: string
                        startTime:
                          type: string
                          format: date-time
                  activePlaybacks:
                    type: array
                    items:
                      type: object
                      properties:
                        jobId:
                          type: string
                        sourceProfile:
                          type: string
                        destinationUrl:
                          type: string
                        duration:
                          type: integer
                        startTime:
                          type: string
                          format: date-time
`;

const swaggerDoc = yaml.load(swaggerYaml);

// ----------------------------------------------------
// Helper: parse "udp://224.0.0.200:10001?interface=en0"
// to get ip=224.0.0.200, port=10001, interface=en0
// ----------------------------------------------------
function parseUdpUrl(urlString) {
  try {
    const u = new URL(urlString);
    if (u.protocol !== 'udp:') {
      return null;
    }
    const ip = u.hostname;
    const port = u.port || '10001';
    const iface = u.searchParams.get('interface') || 'en0';
    return { ip, port, iface };
  } catch (e) {
    return null;
  }
}

// ----------------------------------------------------
// SQLite initialization
// ----------------------------------------------------
const db = new sqlite3.Database('media_jobs.db');
db.serialize(() => {
  // Manager tables
  db.run(`
    CREATE TABLE IF NOT EXISTS recordings (
      id TEXT PRIMARY KEY,
      sourceUrl TEXT,
      duration INTEGER,
      destinationProfile TEXT,
      startTime TEXT,
      endTime TEXT,
      status TEXT,
      processPid INTEGER
    )
  `);
  db.run(`
    CREATE TABLE IF NOT EXISTS playbacks (
      id TEXT PRIMARY KEY,
      sourceProfile TEXT,
      destinationUrl TEXT,
      duration INTEGER,
      startTime TEXT,
      endTime TEXT,
      status TEXT,
      processPid INTEGER
    )
  `);
  db.run(`
    CREATE TABLE IF NOT EXISTS pools (
      id TEXT PRIMARY KEY,
      bucketName TEXT,
      accessKey TEXT,
      secretKey TEXT
    )
  `);
  db.run(`
    CREATE TABLE IF NOT EXISTS assets (
      id TEXT PRIMARY KEY,
      poolId TEXT,
      metadata TEXT
    )
  `);

  // Agent tables
  db.run(`
    CREATE TABLE IF NOT EXISTS agent_recordings (
      jobId TEXT PRIMARY KEY,
      sourceUrl TEXT,
      duration INTEGER,
      destinationProfile TEXT,
      status TEXT,
      processPid INTEGER,
      startTime TEXT
    )
  `);
  db.run(`
    CREATE TABLE IF NOT EXISTS agent_playbacks (
      jobId TEXT PRIMARY KEY,
      sourceProfile TEXT,
      destinationUrl TEXT,
      duration INTEGER,
      status TEXT,
      processPid INTEGER,
      startTime TEXT
    )
  `);
});

// Keep track of setTimeout handles in memory so we can auto-stop
// after the given durations. In production, consider a more robust
// job scheduler or external watchdog. If the server restarts, these
// references are lost.
const activeTimers = {
  recordings: new Map(), // jobId => timeoutHandle
  playbacks: new Map()   // jobId => timeoutHandle
};

// ----------------------------------------------------
// Express Setup
// ----------------------------------------------------
const app = express();
app.use(express.json());

// Serve the Swagger UI at /api-docs
app.use('/api-docs', swaggerUi.serve, swaggerUi.setup(swaggerDoc));

// ===============================
// MANAGER ENDPOINTS (/v1)
// ===============================
const managerRouter = express.Router();

// --------------- RECORDINGS ---------------
managerRouter.post('/recordings', (req, res) => {
  const { sourceUrl, duration, destinationProfile } = req.body;
  const recordingId = `rec-${uuidv4()}`;
  const now = new Date();
  const endTime = new Date(now.getTime() + duration * 1000);

  db.run(`
    INSERT INTO recordings (id, sourceUrl, duration, destinationProfile, startTime, endTime, status, processPid)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
    [
      recordingId,
      sourceUrl,
      duration,
      destinationProfile,
      now.toISOString(),
      endTime.toISOString(),
      'active',
      null
    ],
    (err) => {
      if (err) {
        return res.status(500).json({ error: err.message });
      }

      console.log('About to call Agent with fetch, recordingId=', recordingId);

      fetch('http://localhost:3000/v1/agent/jobs/recordings', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          jobId: recordingId,
          sourceUrl,
          duration,
          destinationProfile
        })
      })
        .then(agentResp => agentResp.json().catch(() => ({})))
        .then(agentData => {
          // We can optionally store agentData.pid into our DB if we want
          // but for now, we just return the Manager's response
          return res.status(201).json({ recordingId });
        })
        .catch(err2 => {
          console.error('Error calling Agent endpoint:', err2);
          // We already inserted the DB record, so decide how to handle:
          return res.status(201).json({
            recordingId,
            warning: 'Recorded in Manager DB, but Agent spawn failed. Check logs.'
          });
        });
    }
  );
});

managerRouter.get('/recordings', (req, res) => {
  db.all(`SELECT * FROM recordings`, (err, rows) => {
    if (err) return res.status(500).json({ error: err.message });
    res.json(rows);
  });
});

managerRouter.get('/recordings/:recordingId', (req, res) => {
  const { recordingId } = req.params;
  db.get(`SELECT * FROM recordings WHERE id = ?`, [recordingId], (err, row) => {
    if (err) return res.status(500).json({ error: err.message });
    if (!row) return res.status(404).json({ message: 'Not found' });
    res.json(row);
  });
});

managerRouter.delete('/recordings/:recordingId', (req, res) => {
  const { recordingId } = req.params;
  db.get(`SELECT * FROM recordings WHERE id = ?`, [recordingId], (err, row) => {
    if (err) return res.status(500).json({ error: err.message });
    if (!row) return res.status(404).json({ message: 'Not found' });

    db.run(`UPDATE recordings SET status='canceled' WHERE id=?`, [recordingId], err2 => {
      if (err2) return res.status(500).json({ error: err2.message });
      // kill child if any
      if (row.processPid) {
        try {
          process.kill(row.processPid, 'SIGTERM');
        } catch { }
      }
      return res.sendStatus(204);
    });
  });
});

// --------------- POOLS ---------------
managerRouter.post('/pools', (req, res) => {
  const { bucketName, credentials } = req.body;
  if (!credentials) return res.status(400).json({ error: 'Missing credentials' });
  const poolId = `pool-${uuidv4()}`;
  db.run(
    `INSERT INTO pools (id, bucketName, accessKey, secretKey) VALUES (?, ?, ?, ?)`,
    [poolId, bucketName, credentials.accessKey, credentials.secretKey],
    err => {
      if (err) return res.status(500).json({ error: err.message });
      res.status(201).json({ poolId, bucketName });
    }
  );
});

managerRouter.get('/pools', (req, res) => {
  db.all(`SELECT * FROM pools`, (err, rows) => {
    if (err) return res.status(500).json({ error: err.message });
    res.json(rows);
  });
});

// --------------- ASSETS ---------------
managerRouter.get('/pools/:poolId/assets', (req, res) => {
  const { poolId } = req.params;
  db.all(`SELECT * FROM assets WHERE poolId=?`, [poolId], (err, rows) => {
    if (err) return res.status(500).json({ error: err.message });
    res.json(rows);
  });
});

managerRouter.delete('/pools/:poolId/assets', (req, res) => {
  const { poolId } = req.params;
  const { assetId } = req.query;
  db.get(
    `SELECT * FROM assets WHERE id=? AND poolId=?`,
    [assetId, poolId],
    (err, row) => {
      if (err) return res.status(500).json({ error: err.message });
      if (!row) return res.status(404).json({ message: 'Asset not found' });
      db.run(
        `DELETE FROM assets WHERE id=? AND poolId=?`,
        [assetId, poolId],
        err2 => {
          if (err2) return res.status(500).json({ error: err2.message });
          res.sendStatus(204);
        }
      );
    }
  );
});

managerRouter.get('/assets/:assetId', (req, res) => {
  const { assetId } = req.params;
  db.get(`SELECT * FROM assets WHERE id=?`, [assetId], (err, row) => {
    if (err) return res.status(500).json({ error: err.message });
    if (!row) return res.status(404).json({ message: 'Asset not found' });
    const data = {
      assetId: row.id,
      poolId: row.poolId,
      metadata: JSON.parse(row.metadata || '{}')
    };
    res.json(data);
  });
});

// --------------- PLAYBACKS ---------------
managerRouter.post('/playbacks', (req, res) => {
  const { sourceProfile, destinationUrl, duration } = req.body;
  const playbackId = `play-${uuidv4()}`;
  const now = new Date();
  const endTime = duration > 0 ? new Date(now.getTime() + duration * 1000) : null;

  db.run(
    `INSERT INTO playbacks (id, sourceProfile, destinationUrl, duration, startTime, endTime, status, processPid)
     VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
    [
      playbackId,
      sourceProfile,
      destinationUrl,
      duration,
      now.toISOString(),
      endTime ? endTime.toISOString() : null,
      'active',
      null
    ],
    err => {
      if (err) return res.status(500).json({ error: err.message });
      res.status(201).json({ playbackId });
    }
  );
});

managerRouter.get('/playbacks', (req, res) => {
  db.all(`SELECT * FROM playbacks`, (err, rows) => {
    if (err) return res.status(500).json({ error: err.message });
    res.json(rows);
  });
});

managerRouter.get('/playbacks/:playbackId', (req, res) => {
  const { playbackId } = req.params;
  db.get(`SELECT * FROM playbacks WHERE id=?`, [playbackId], (err, row) => {
    if (err) return res.status(500).json({ error: err.message });
    if (!row) return res.status(404).json({ message: 'Playback not found' });
    res.json(row);
  });
});

managerRouter.delete('/playbacks/:playbackId', (req, res) => {
  const { playbackId } = req.params;
  db.get(`SELECT * FROM playbacks WHERE id=?`, [playbackId], (err, row) => {
    if (err) return res.status(500).json({ error: err.message });
    if (!row) return res.status(404).json({ message: 'Playback not found' });
    db.run(`UPDATE playbacks SET status='canceled' WHERE id=?`, [playbackId], err2 => {
      if (err2) return res.status(500).json({ error: err2.message });
      if (row.processPid) {
        try {
          process.kill(row.processPid, 'SIGTERM');
        } catch { }
      }
      res.sendStatus(204);
    });
  });
});

// --------------- ADMIN ---------------
managerRouter.get('/admin/stats', (req, res) => {
  // Summaries
  db.all(`SELECT COUNT(*) AS cnt FROM recordings WHERE status='active'`, (err, rec) => {
    if (err) return res.status(500).json({ error: err.message });
    const concurrentRecordings = rec[0].cnt;

    db.all(`SELECT COUNT(*) AS cnt FROM playbacks WHERE status='active'`, (err2, pb) => {
      if (err2) return res.status(500).json({ error: err2.message });
      const concurrentPlaybacks = pb[0].cnt;

      db.all(`SELECT COUNT(*) AS cnt FROM assets`, (err3, as) => {
        if (err3) return res.status(500).json({ error: err3.message });
        const totalAssets = as[0].cnt;

        const systemLoad = 0.2;
        const clusterUsage = { agentCount: 1 };

        res.json({
          concurrentRecordings,
          concurrentPlaybacks,
          totalAssets,
          systemLoad,
          clusterUsage
        });
      });
    });
  });
});

// Bind the managerRouter under /v1
app.use('/v1', managerRouter);

// ===============================
// AGENT ENDPOINTS (/v1/agent)
// ===============================
const agentRouter = express.Router();

/**
 * Helper to spawn udp-to-hls with appropriate args
 * Returns: childProcess
 */
function spawnUdpToHls(jobId, sourceUrl, duration) {
  // Attempt to parse the sourceUrl as a UDP URL
  const parsed = parseUdpUrl(sourceUrl);
  if (!parsed) {
    // Fallback: you might handle SRT or other protocols
    // For now, we’ll just throw
    throw new Error(`Invalid or non-UDP sourceUrl: ${sourceUrl}`);
  }

  const { ip, port, iface } = parsed;
  // Example invocation, adjust arguments as needed.
  // We’ll store segments in a subdirectory named by jobId.
  // You may add other flags for S3 endpoints, etc.
  const args = [
    '-n', iface,
    '-v', '2',
    '--duration', duration,
    '--hls_keep_segments', '10',
    '-i', ip,
    '-p', port,
    '-o', jobId  // acts as the "channel name"/local output folder
  ];

  console.log(`Spawning udp-to-hls => ../bin/udp-to-hls ${args.join(' ')}`);
  const child = spawn('../bin/udp-to-hls', args, {
    stdio: ['ignore', 'pipe', 'pipe']
  });
  return child;
}

/**
 * Helper to spawn hls-to-udp in VOD or live mode
 * Returns: childProcess
 */
function spawnHlsToUdp(jobId, sourceProfile, destinationUrl, vodStartTime, vodEndTime) {
  if (!jobId) {
    console.log('Invalid jobId:', jobId);
    return Array();
  }

  // We parse the 'destinationUrl' to get IP:port
  // e.g. "udp://239.1.1.1:5000" or "udp://224.0.0.200:10001"
  const parsedDest = parseUdpUrl(destinationUrl);
  if (!parsedDest) {
    throw new Error(`Invalid or non-UDP destinationUrl: ${destinationUrl}`);
  }
  const { ip, port } = parsedDest;

  const baseArgs = [];
  let isVod = false;

  // parse ./hourly_urls.log for the vodStartTime/vodEndTime range and make an array of playlists to play
  let vodPlaylists = [];
  if (vodStartTime && vodEndTime && (vodStartTime != "" || vodEndTime != "")) {
    /* confirm the vodEndTime is greater than vodStartTime and they are in the right date/time format */
    /* convert the date/time strings to date objects */
    /* confirm they are the right format */
    if (new Date(vodEndTime) <= new Date(vodStartTime)) {
      console.log('Invalid vodEndTime, vodEndTime:', vodEndTime, ' <= vodStartTime:', vodStartTime);
      return Array();
    }
    if (!new Date(vodStartTime).toISOString().includes(vodStartTime)) {
      console.log('Invalid vodStartTime, vodStartTime:', vodStartTime);
      return Array();
    }
    if (!new Date(vodEndTime).toISOString().includes(vodEndTime)) {
      console.log('Invalid vodEndTime, vodEndTime:', vodEndTime);
      return Array();
    }
    // open the ./hourly_urls.log file and read the lines
    const lines = fs.readFileSync('./hourly_urls.log', 'utf-8').split('\n');
    for (const line of lines) {
      if (!line) continue;
      /* check if starts with Hour */
      if (!line.startsWith('Hour')) continue;

      //console.log('hls playback hour log line:', line);

      /* parse format as `Hour test69/2025/03/17/13 => http://127.0.0.1:9000/hls/test69/2025/03/17/13/index.m3u8?auth_string_key`above */
      const [hour, url] = line.split(' => ');
      if (!hour || !url) {
        console.log('Invalid hour, url line:', line);
        continue
      }
      const [jobId_prefix, year, month, day, hourStr] = hour.split('/');
      const [_, newJobId] = jobId_prefix.split(' ');
      if (!hourStr) {
        console.log('Invalid date, hourStr:', hourStr);
        continue;
      }
      if (newJobId !== jobId) {
        //console.debug('Not the requested jobId, skipping: ', jobId, ' != ', newJobId);
        continue;
      }
      /* check if the hour is within the range */
      const hourDate = new Date(`${year}-${month}-${day}T${hourStr}:00:00Z`);
      if (hourDate >= new Date(vodStartTime) && hourDate <= new Date(vodEndTime)) {
        console.log('Using hls playback url:', url);
        vodPlaylists.push(url);
      }
    }
  } else {
    // If no times are given, just get all the urls from the playlist file
    const lines = fs.readFileSync('./hourly_urls.log', 'utf-8').split('\n');
    for (const line of lines) {
      if (!line) continue;
      if (line.startsWith('Hour')) {
        const [hour, url] = line.split(' => ');
        if (!hour || !url) {
          console.log('Invalid hour, url line:', line);
          continue;
        }
        /* Confirm we have the right jobId */
        const [jobId_prefix, year, month, day, hourStr] = hour.split('/');
        const [_, newJobId] = jobId_prefix.split(' ');
        if (!hourStr) {
          console.log('Invalid date, hourStr:', date, hourStr);
          continue;
        }
        if (newJobId !== jobId) {
          //console.debug('Not the requested jobId, skipping: ', jobId, ' != ', newJobId);
          continue;
        }
        if (url) {
          vodPlaylists.push(url);
          console.log('using hls playback url:', url);
        }
      }
    }
  }

  /* array of child pids to return */
  let childArray = [];

  /* go through each playlist and feed it to the hls-to-udp */
  for (const vodPlaylist of vodPlaylists) {
    // If we have a start/end time, assume we want multi-hour VOD mode
    let baseArgs = [];
    baseArgs.push('--vod');
    baseArgs.push('--use-smoother');
    baseArgs.push('-v', '2');
    baseArgs.push('-u', `${vodPlaylist}`);

    // Output
    baseArgs.push('-o', `${ip}:${port}`);

    console.log(`Spawning hls-to-udp => ../bin/hls-to-udp ${baseArgs.join(' ')}`);
    /* fill childArray per spawn */
    const child = spawn('../bin/hls-to-udp', baseArgs, {
      stdio: ['ignore', 'pipe', 'pipe']
    });
    childArray.push(child);
  }

  return childArray;
}

// GET status
agentRouter.get('/status', (req, res) => {
  db.all(`SELECT * FROM agent_recordings`, (err, recRows) => {
    if (err) return res.status(500).json({ error: err.message });
    db.all(`SELECT * FROM agent_playbacks`, (err2, pbRows) => {
      if (err2) return res.status(500).json({ error: err2.message });
      res.json({
        agentId: 'agent-001',
        status: 'healthy',
        activeRecordings: recRows,
        activePlaybacks: pbRows
      });
    });
  });
});

// Start a recording job
agentRouter.post('/jobs/recordings', (req, res) => {
  const { jobId, sourceUrl, duration, destinationProfile } = req.body;
  if (!jobId || !sourceUrl) {
    return res.status(400).json({ error: 'Missing jobId or sourceUrl' });
  }
  let child;
  try {
    child = spawnUdpToHls(jobId, sourceUrl, duration);
  } catch (spawnErr) {
    return res.status(400).json({ error: spawnErr.message });
  }
  const pid = child.pid;
  const now = new Date();

  // Insert into DB
  db.run(
    `INSERT INTO agent_recordings (jobId, sourceUrl, duration, destinationProfile, status, processPid, startTime)
     VALUES (?, ?, ?, ?, ?, ?, ?)`,
    [jobId, sourceUrl, duration, destinationProfile, 'running', pid, now.toISOString()],
    err => {
      if (err) {
        try { process.kill(pid, 'SIGTERM'); } catch { }
        return res.status(500).json({ error: err.message });
      }

      // If duration > 0, auto-stop after duration
      if (duration && duration > 0) {
        const timer = setTimeout(() => {
          console.log(`Auto-stopping recording jobId=${jobId} after duration`);
          // Stop the process
          try { process.kill(pid, 'SIGTERM'); } catch { }
          db.run(`DELETE FROM agent_recordings WHERE jobId=?`, [jobId]);
          activeTimers.recordings.delete(jobId);
        }, (duration + 1) * 1000);
        activeTimers.recordings.set(jobId, timer);
      }

      child.on('close', code => {
        console.log(`Recording jobId=${jobId} ended with code=${code}`);
      });
      res.status(201).json({ message: 'Recording job accepted', pid });
    }
  );
});

// Stop a recording job
agentRouter.delete('/recordings/:jobId', (req, res) => {
  const { jobId } = req.params;
  db.get(`SELECT * FROM agent_recordings WHERE jobId=?`, [jobId], (err, row) => {
    if (err) return res.status(500).json({ error: err.message });
    if (!row) return res.status(404).json({ message: 'Not found' });
    if (row.processPid) {
      try {
        process.kill(row.processPid, 'SIGTERM');
      } catch { }
    }
    db.run(`DELETE FROM agent_recordings WHERE jobId=?`, [jobId]);
    // Clear any setTimeout
    if (activeTimers.recordings.has(jobId)) {
      clearTimeout(activeTimers.recordings.get(jobId));
      activeTimers.recordings.delete(jobId);
    }
    res.sendStatus(204);
  });
});

// Start a playback job
agentRouter.post('/jobs/playbacks', (req, res) => {
  const { jobId, sourceProfile, destinationUrl, duration, vodStartTime, vodEndTime } = req.body;
  if (!jobId || !destinationUrl) {
    return res.status(400).json({ error: 'Missing jobId or destinationUrl' });
  }
  let childArray;
  try {
    childArray = spawnHlsToUdp(jobId, sourceProfile, destinationUrl, vodStartTime, vodEndTime);
  } catch (spawnErr) {
    return res.status(400).json({ error: spawnErr.message });
  }
  if (!childArray) {
    return res.status(400).json({ error: 'Invalid spawn' });
  }
  /* check if empty */
  if (childArray.length == 0) {
    return res.status(400).json({ error: 'Invalid spawn' });
  }
  const now = new Date();
  let completedInserts = 0;
  const pids = [];

  // Insert each child of the array into the DB
  for (const child of childArray) {
    const pid = child.pid;
    pids.push(pid);

    db.run(
      `INSERT INTO agent_playbacks (jobId, sourceProfile, destinationUrl, duration, status, processPid, startTime)
      VALUES (?, ?, ?, ?, ?, ?, ?)`,
      [jobId, sourceProfile, destinationUrl, duration, 'running', pid, now.toISOString()],
      err => {
        if (err) {
          try { process.kill(pid, 'SIGTERM'); } catch { }
          return res.status(500).json({ error: err.message });
        }

        // If duration > 0, auto-stop
        if (duration && duration > 0) {
          const timer = setTimeout(() => {
            console.log(`Auto-stopping playback jobId=${jobId} after duration`);
            try { process.kill(pid, 'SIGTERM'); } catch { }
            db.run(`DELETE FROM agent_playbacks WHERE jobId=?`, [jobId]);
            activeTimers.playbacks.delete(jobId);
          }, duration * 1000);
          activeTimers.playbacks.set(jobId, timer);
        } else {
          /* get the duration from the db */
          db.get(`SELECT duration FROM agent_playbacks WHERE jobId=?`, [jobId], (err, row) => {
            if (err) return res.status(500).json({ error: err.message });
            if (!row) return res.status(404).json({ message: 'Not found' });
            const duration = row.duration;
            if (duration && duration > 0) {
              const timer = setTimeout(() => {
                console.log(`Auto-stopping playback jobId=${jobId} after duration`);
                try { process.kill(pid, 'SIGTERM'); } catch { }
                db.run(`DELETE FROM agent_playbacks WHERE jobId=?`, [jobId]);
                activeTimers.playbacks.delete(jobId);
              }, duration * 1000);
              activeTimers.playbacks.set(jobId, timer);
            }
          });
        }
        completedInserts++;
        // Once we've inserted them all, we can respond
        if (completedInserts === childArray.length) {
          res.status(201).json({
            message: 'Playback job accepted',
            childCount: childArray.length,
            pids
          });
        }
      }
    );
  }
});

// Stop a playback job
agentRouter.delete('/playbacks/:jobId', (req, res) => {
  const { jobId } = req.params;
  db.get(`SELECT * FROM agent_playbacks WHERE jobId=?`, [jobId], (err, row) => {
    if (err) return res.status(500).json({ error: err.message });
    if (!row) return res.status(404).json({ message: 'Not found' });
    if (row.processPid) {
      try {
        process.kill(row.processPid, 'SIGTERM');
      } catch { }
    }
    db.run(`DELETE FROM agent_playbacks WHERE jobId=?`, [jobId]);
    // clear any setTimeout
    if (activeTimers.playbacks.has(jobId)) {
      clearTimeout(activeTimers.playbacks.get(jobId));
      activeTimers.playbacks.delete(jobId);
    }
    res.sendStatus(204);
  });
});

// Bind agentRouter under /v1/agent
app.use('/v1/agent', agentRouter);

// ----------------------------------------------------
// Start the server
// ----------------------------------------------------
const PORT = 3000;
app.listen(PORT, () => {
  console.log(`Server running on port ${PORT}. Swagger at http://localhost:${PORT}/api-docs`);
});