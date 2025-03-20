/****************************************************
 * server.js — Recording/Playback API Server
 * 
 * Environment Variables:
 * - SERVER_PORT: Port for the server to listen on (default: 3000)
 * - SERVER_HOST: Host for the server to listen on (default: 127.0.0.1)
 * - AWS_S3_ENDPOINT: Endpoint for the S3 server (default: http://127.0.0.1:9000)
 * - SMOOTHER_LATENCY: Smoother latency for hls-to-udp (default: 500)
 * - VERBOSE: Verbosity level for hls-to-udp (default: 2)
 * - UDP_BUFFER_BYTES: Buffer size for hls-to-udp (default: 0)
 * 
 * Usage:
 * - Start the server with `node server.js`
 * - Access the Swagger UI at http://localhost:3000/api-docs
 * - Use the API to create recordings and playbacks
 * - The server will spawn the appropriate agents to handle the jobs
 * - The agents will spawn the appropriate processes to handle the jobs
 * - The server will store the job status in the SQLite database
 * - The server will auto-stop the jobs after the given durations
 * - The server will store the recording URLs in the SQLite database
 * - The server will serve the recording URLs to the clients
 * 
 * URL parameters for UDP MpegTS:
 * - udp://multicast_ip:port?interface=net1
 * 
 * Dependencies:
 * - express: Web server framework
 * - sqlite3: SQLite database
 * - udp-to-hls: UDP to HLS converter
 * - hls-to-udp: HLS to UDP converter
 * - MinIO: S3-compatible server
 * - node-fetch: Fetch API for Node.js
 * 
 * Build Instructions:
 * - Install Node.js and npm version greater than 18 ideally
 * - Run `npm install` to install dependencies
 * - Build the udp-to-hls and hls-to-udp in ../ one directory down using `make && make install`
 * - Run `node server.js` to start the server
 * 
 * - Author: CK <ck@groovybits> https://github.com/groovybits/mpegts_to_s3
 * 
 ****************************************************/

const serverVersion = '1.0.26';

const express = require('express');
const { v4: uuidv4 } = require('uuid');
const sqlite3 = require('sqlite3').verbose();
const { spawn } = require('child_process');

const fs = require('fs');

// For Swagger
const swaggerUi = require('swagger-ui-express');
const yaml = require('js-yaml');

// For fetch
const fetch = require('node-fetch');
const { env } = require('process');

// Server URL Base
const SERVER_PORT = process.env.SERVER_PORT || 3000;
const SERVER_HOST = process.env.SERVER_HOST || "127.0.0.1";
const serverUrl = 'http://' + SERVER_HOST + ':' + SERVER_PORT;
const s3endPoint = process.env.AWS_S3_ENDPOINT || 'http://127.0.0.1:9000';

// check env and set the values for the baseargs, else set to defaults, use vars below
const SMOOTHER_LATENCY = process.env.SMOOTHER_LATENCY || 500;
const PLAYBACK_VERBOSE = process.env.VERBOSE || 2;
const RECORDING_VERBOSE = process.env.VERBOSE || 1;
const UDP_BUFFER_BYTES = process.env.UDP_BUFFER_BYTES || 0;
const SWAGGER_FILE = process.env.SWAGGER_FILE || 'swagger.yaml';

// ----------------------------------------------------
// Swagger YAML read in and parsed
// ----------------------------------------------------
const swaggerYaml = fs.readFileSync(SWAGGER_FILE, 'utf8');
const swaggerDoc = yaml.load(swaggerYaml);

// ----------------------------------------------------
// Helper: parse "udp://224.0.0.200:10001?interface=net1"
// to get ip=224.0.0.200, port=10001, interface=net1
// ----------------------------------------------------
function parseUdpUrl(urlString) {
  try {
    const u = new URL(urlString);
    if (u.protocol !== 'udp:') {
      return null;
    }
    const ip = u.hostname;
    const port = u.port || '4102';
    const iface = u.searchParams.get('interface') || 'net1';
    return { ip, port, iface };
  } catch (e) {
    return null;
  }
}

/**
 * Helper to read hourly_urls.log and store recording URLs into DB for a given jobId.
 */
function storeRecordingUrls(jobId) {
  try {
    const fileData = fs.readFileSync('./hourly_urls.log', 'utf-8');
    const lines = fileData.split('\n');
    for (const line of lines) {
      if (!line.startsWith('Hour')) continue;
      const parts = line.split(' => ');
      if (parts.length !== 2) continue;
      const hourPart = parts[0]; // e.g., "Hour job20/2025/03/18/10"
      const url = parts[1];
      const tokens = hourPart.split(' ');
      if (tokens.length < 2) continue;
      const jobDatePart = tokens[1]; // "job20/2025/03/18/10"
      const jobIdFromFile = jobDatePart.split('/')[0];
      if (jobIdFromFile !== jobId) continue;
      const hourString = jobDatePart.substring(jobIdFromFile.length + 1); // "2025/03/18/10"
      db.run(
        `INSERT OR REPLACE INTO recording_urls (jobId, hour, url) VALUES (?, ?, ?)`,
        [jobId, hourString, url.trim()],
        function (err) {
          if (err) {
            console.error('Error inserting recording url into DB for job', jobId, ':', err.message);
          } else {
            console.log('Inserted recording url ', url.trim(), ' into DB for job:', jobId, 'hour:', hourString);
          }
        }
      );
    }
  } catch (err) {
    console.error('Error reading hourly_urls.log:', err.message);
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
  // Playbacks table
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
  // Pools table
  db.run(`
    CREATE TABLE IF NOT EXISTS pools (
      id TEXT PRIMARY KEY,
      bucketName TEXT,
      accessKey TEXT,
      secretKey TEXT
    )
  `);
  // Assets table
  db.run(`
    CREATE TABLE IF NOT EXISTS assets (
      id TEXT PRIMARY KEY,
      poolId TEXT,
      metadata TEXT
    )
  `);
  // Recording URLs table
  db.run(`
    CREATE TABLE IF NOT EXISTS recording_urls (
      jobId TEXT,
      hour TEXT,
      url TEXT,
      PRIMARY KEY (jobId, hour)
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
  // Agent playbacks table
  db.run(`
    CREATE TABLE IF NOT EXISTS agent_playbacks (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      jobId TEXT,
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

      let fetchUrl = serverUrl + "/v1/agent/jobs/recordings";
      console.log('About to call Agent with fetch, recordingId=', recordingId, ' Url=', sourceUrl, ' Duration=', duration, ' DestinationProfile=', destinationProfile, ' FetchUrl=', fetchUrl);

      fetch(fetchUrl, {
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
  const endTime = duration > 0 ? new Date(now.getTime() + (duration + 3) * 1000) : null;

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
    }
  );
  // Call the agent to start the playback
  let fetchUrl = serverUrl + "/v1/agent/jobs/playbacks";
  console.log('About to call Agent with fetch, playbackId=', playbackId, ' SourceProfile=', sourceProfile, ' DestinationUrl=', destinationUrl, ' Duration=', duration, ' FetchUrl=', fetchUrl);
  fetch(fetchUrl, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      jobId: playbackId,
      sourceProfile,
      destinationUrl,
      duration
    })
  })
    .then(agentResp => agentResp.json().catch(() => ({})))
    .then(agentData => {
      // We can optionally store agentData.pid into our DB if we want
      // but for now, we just return the Manager's response
      return res.status(201).json({ playbackId });
    })
    .catch(err2 => {
      console.error('Error calling Agent endpoint:', err2);
      // We already inserted the DB record, so decide how to handle:
      return res.status(201).json({
        playbackId,
        warning: 'Recorded in Manager DB, but Agent spawn failed. Check logs.'
      });
    });
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
function spawnUdpToHls(jobId, sourceUrl, duration, s3bucketName) {
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
    '-v', `${RECORDING_VERBOSE}`,
    '-e', s3endPoint,
    '-b', s3bucketName,
    //'--duration', duration,
    '--hls_keep_segments', '0',
    '-i', ip,
    '-p', port,
    '-o', jobId  // acts as the "channel name"/local output folder
  ];

  console.log(`Spawning udp-to-hls => ../bin/udp-to-hls ${args.join(' ')}`);
  const child = spawn('../bin/udp-to-hls', args, {
    stdio: ['ignore', 'pipe', 'pipe']
  });

  // Attach listeners for stdout and stderr
  child.stdout.on('data', data => {
    const output = data.toString().trim();
    console.log(`udp-to-hls stdout: ${output}`);
    // If output starts with "Hour", parse and store in DB
    if (output.startsWith('Hour')) {
      // Expected format: "Hour <jobId>/<year>/<month>/<day>/<hourStr> => <url>"
      const parts = output.split(' => ');
      if (parts.length === 2) {
        const hourPart = parts[0]; // e.g., "Hour rec-xxxx/yyyy/mm/dd/hh:mm:ss"
        const url = parts[1];
        // Remove "Hour " prefix
        const hourInfo = hourPart.substring(5).trim();
        const firstSlash = hourInfo.indexOf('/');
        if (firstSlash !== -1) {
          const currentJobId = hourInfo.substring(0, firstSlash);
          const hourString = hourInfo.substring(firstSlash + 1); // "yyyy/mm/dd/hh:mm:ss"
          db.run(
            `INSERT OR REPLACE INTO recording_urls (jobId, hour, url) VALUES (?, ?, ?)`,
            [currentJobId, hourString, url],
            function (err) {
              if (err) {
                console.error('Error inserting recording url into DB:', err.message);
              } else {
                console.log('Inserted recording url into DB for job:', currentJobId, 'hour:', hourString);
              }
            }
          );
        }
      }
    }
  });
  child.stderr.on('data', data => {
    console.error(`udp-to-hls stderr: ${data.toString().trim()}`);
  });

  return child;
}

async function getVodPlaylists(jobId, vodStartTime, vodEndTime) {
  return new Promise((resolve, reject) => {
    db.all(`SELECT hour, url FROM recording_urls WHERE jobId = ?`, [jobId], (err, rows) => {
      if (err) {
        return reject(err);
      }
      const vodPlaylists = [];
      for (const row of rows) {
        // row.hour is expected in format "yyyy/mm/dd/hh:mm:ss"
        const dateParts = row.hour.split('/');
        if (dateParts.length < 4) {
          console.log('Invalid date format in recording_urls:', row.hour);
          continue;
        }
        const isoString = `${dateParts[0]}-${dateParts[1]}-${dateParts[2]}T${dateParts[3]}Z`;
        const hourDate = new Date(isoString);
        if (vodStartTime && vodEndTime && (vodStartTime !== "" || vodEndTime !== "")) {
          if (hourDate >= new Date(vodStartTime) && hourDate <= new Date(vodEndTime)) {
            console.log('Using hls playback url:', row.url);
            vodPlaylists.push(row.url);
          }
        } else {
          console.log('Using hls playback url:', row.url);
          vodPlaylists.push(row.url);
        }
      }
      resolve(vodPlaylists);
    });
  });
}

/**
 * Helper to spawn hls-to-udp in VOD or live mode
 * If multiple playlists exist, it waits for only the first process to finish before returning.
 * Returns: Promise resolving to an array of spawned child processes.
 */
async function spawnHlsToUdp(jobId, sourceProfile, destinationUrl, vodStartTime, vodEndTime) {
  if (!jobId) {
    console.log('Invalid jobId:', jobId);
    return [];
  }

  // Parse the destinationUrl to get IP and port
  const parsedDest = parseUdpUrl(destinationUrl);
  if (!parsedDest) {
    throw new Error(`Invalid or non-UDP destinationUrl: ${destinationUrl}`);
  }
  const { ip, port } = parsedDest;

  // Build the vodPlaylists array by querying the recording_urls table in the DB
  let vodPlaylists = [];
  try {
    vodPlaylists = await getVodPlaylists(jobId, vodStartTime, vodEndTime);
  } catch (err) {
    console.error('Error getting vod playlists from DB:', err.message);
    return [];
  }

  let childArray = [];
  // Spawn each hls-to-udp process.
  // Wait only for the first process if there is more than one playlist.
  for (let i = 0; i < vodPlaylists.length; i++) {
    let baseArgs = [];
    baseArgs.push('--vod');
    baseArgs.push('--use-smoother');
    baseArgs.push('-l', `${SMOOTHER_LATENCY}`);
    baseArgs.push('-b', `${UDP_BUFFER_BYTES}`);
    baseArgs.push('-v', `${PLAYBACK_VERBOSE}`);
    baseArgs.push('-u', `${vodPlaylists[i]}`);
    baseArgs.push('-o', `${ip}:${port}`);

    console.log(`Spawning hls-to-udp => ../bin/hls-to-udp ${baseArgs.join(' ')}`);
    const child = spawn('../bin/hls-to-udp', baseArgs, {
      stdio: ['ignore', 'pipe', 'pipe']
    });
    childArray.push(child);

    // Attach listeners for stdout and stderr
    child.stdout.on('data', data => {
      console.log(`hls-to-udp stdout: ${data.toString().trim()}`);
    });
    child.stderr.on('data', data => {
      console.error(`hls-to-udp stderr: ${data.toString().trim()}`);
    });

    // Only wait for the first process if there is more than one playlist
    if (i === 0 && vodPlaylists.length > 1) {
      await new Promise((resolve, reject) => {
        child.on('close', code => {
          console.log(`First hls-to-udp process ended with code=${code}`);
          resolve();
        });
        child.on('error', err => {
          reject(err);
        });
      });
    }
  }
  console.log('vodPlaylist: Launched', childArray.length, 'hls-to-udp processes (first one waited for)');
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
    child = spawnUdpToHls(jobId, sourceUrl, duration, destinationProfile);
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
          console.log(`Auto-stopping recording jobId=${jobId} after duration=${duration}`);
          // Stop the process
          try { process.kill(pid, 'SIGTERM'); } catch { }
          // Store the recording URLs in DB from the hourly_urls.log file
          storeRecordingUrls(jobId);
          db.run(`DELETE FROM agent_recordings WHERE jobId=?`, [jobId]);
          activeTimers.recordings.delete(jobId);
        }, (duration + 60.0) * 1000); // Add up to 60s buffer for GOP alignment or other delays
        activeTimers.recordings.set(jobId, timer);
      }

      child.on('close', code => {
        console.log(`Recording jobId=${jobId} ended with code=${code} after duration=${duration}`);
        // Store the recording URLs in DB from the hourly_urls.log file
        storeRecordingUrls(jobId);
        db.run(`DELETE FROM agent_recordings WHERE jobId=?`, [jobId]);
        activeTimers.recordings.delete
          (jobId);
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
agentRouter.post('/jobs/playbacks', async (req, res) => {
  const { jobId, sourceProfile, destinationUrl, duration, vodStartTime, vodEndTime } = req.body;
  if (!jobId || !destinationUrl) {
    return res.status(400).json({ error: 'Missing jobId or destinationUrl' });
  }
  let childArray;
  try {
    childArray = await spawnHlsToUdp(jobId, sourceProfile, destinationUrl, vodStartTime, vodEndTime);
  } catch (spawnErr) {
    return res.status(400).json({ error: spawnErr.message });
  }
  if (!childArray || childArray.length === 0) {
    return res.status(400).json({ error: 'Invalid spawn' });
  }
  /* check if empty */
  if (childArray.length == 0) {
    return res.status(400).json({ error: 'Invalid spawn' });
  }
  const now = new Date();
  let completedInserts = 0;
  const pids = [];
  let errors = 0;

  // Insert each child of the array into the DB
  for (const child of childArray) {
    const pid = child.pid;
    pids.push(pid);

    // Check if the jobId is already playing back. Silently proceed if not found.
    db.get(`SELECT * FROM agent_playbacks WHERE jobId=?`, [jobId], (err, row) => {
      if (err) {
        // see if the jobid is not int the db, if so then just let us process and don't return
        console.error('Error checking for existing playback:', err);
        try { process.kill(pid, 'SIGTERM'); }
        catch { }
        errors++;
        return;
      }
      if (row) {
        if (row.status === 'running') {
          // confirm the pid really exists and is still running, if so then return
          try {
            console.warn('Playback job marked already running: jobId=', jobId, 'pid=', row.processPid, ' checking if still running');
            // check if pid is running still
            process.kill(row.processPid, 0);
            // kill the process
            console.warn('Killing existing playback:', row.processPid);
            try {
              process.kill(row.processPid, 'SIGTERM');
            } catch (err) {
              console.log('Error killing existing playback:', err);
            }
            db.run(`DELETE FROM agent_playbacks WHERE jobId=?`, [jobId]);
            console.warn('Deleted existing playback:', jobId);
          } catch {
            // if the process is not running, then delete the row and proceed
            console.log('Playback job already running but not found: jobId=', jobId);
            db.run(`DELETE FROM agent_playbacks WHERE jobId=?`, [jobId]);
          }
        } else {
          db.run(`DELETE FROM agent_playbacks WHERE jobId=?`, [jobId], (deleteErr) => {
            if (deleteErr) {
              console.error('Error deleting existing playback:', deleteErr);
              try { process.kill(pid, 'SIGTERM'); }
              catch { }
              errors++;
              return;
            }
            // Deletion successful; proceed silently.
          });
        }
      }
    });

    db.run(
      `INSERT INTO agent_playbacks (jobId, sourceProfile, destinationUrl, duration, status, processPid, startTime)
      VALUES (?, ?, ?, ?, ?, ?, ?)`,
      [jobId, sourceProfile, destinationUrl, duration, 'running', pid, now.toISOString()],
      err => {
        if (err) {
          try { process.kill(pid, 'SIGTERM'); } catch { }
          console.error('Error inserting playback:', err);
        }

        // If duration > 0, auto-stop
        if (duration && duration > 0) {
          const timer = setTimeout(() => {
            console.log(`Auto-stopping playback jobId=${jobId} after duration`);
            try { process.kill(pid, 'SIGTERM'); } catch { }
            db.run(`DELETE FROM agent_playbacks WHERE jobId=?`, [jobId]);
            activeTimers.playbacks.delete(jobId);
          }, (duration + 3.0) * 1000);
          activeTimers.playbacks.set(jobId, timer);
        } else {
          /* get the duration from the db */
          db.get(`SELECT duration FROM agent_playbacks WHERE jobId=?`, [jobId], (err, row) => {
            if (err) {
              console.error('Error getting duration from db:', err);
              return;
            }
            if (!row) {
              console.error('No row with duration found in db for jobId:', jobId);
              return;
            }
            const durationFull = row.duration;
            if (durationFull && durationFull > 0) {
              const timer = setTimeout(() => {
                console.log(`Auto-stopping playback jobId=${jobId} after duration ${durationFull}`);
                try { process.kill(pid, 'SIGTERM'); } catch { }
                db.run(`DELETE FROM agent_playbacks WHERE jobId=?`, [jobId]);
                activeTimers.playbacks.delete(jobId);
              }, (durationFull + 3.0) * 1000);
              activeTimers.playbacks.set(jobId, timer);
            } else {
              console.log('No duration found in db for jobId:', jobId);
              // just play it forever, no cleanup setup
            }
          });
        }
        completedInserts++;
        // Once we've inserted them all, we can respond
        if (completedInserts === childArray.length) {
          if (completedInserts > 0) {
            child.on('close', code => {
              console.log(`Playback jobId=${jobId} ended with code=${code}`);
              try { process.kill(pid, 'SIGTERM'); }
              catch { }
              db.run(`DELETE FROM agent_playbacks WHERE jobId=?`, [jobId]);
              activeTimers.playbacks.delete(jobId);
            }
            );
            console.error('Inserted all child processes:', completedInserts, ' of ', childArray.length, ' with ', errors, ' errors');
            return res.status(201).json({
              message: 'Playback job accepted',
              childCount: childArray.length,
              pids
            });
          } else {
            return res.status(400).json({ error: ('Only ', completedInserts, ' child processes inserted of ', childArray.length, ' with ', errors, ' errors') });
          }
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
app.listen(SERVER_PORT, () => {
  console.log(`Server version ${serverVersion} running on ${serverUrl}.`);
  let capture_buffer_size = env.CAPTURE_BUFFER_SIZE || `4193904`;
  let segment_duration_ms = env.SEGMENT_DURATION_MS || `2000`;
  let minio_root_user = env.MINIO_ROOT_USER || `minioadmin`;
  let minio_root_password = env.MINIO_ROOT_PASSWORD || `minioadmin`;
  let url_signing_seconds = env.URL_SIGNING_SECONDS || `604800`;
  let use_estimated_duration = env.USE_ESTIMATED_DURATION || `true`;
  let max_segment_size_bytes = env.MAX_SEGMENT_SIZE_BYTES || `5242756`;

  const help_msg = `
Environment Variables:
  - SERVER_PORT: Port for the Node server to listen on (default: ` + SERVER_PORT + `)
  - SERVER_HOST: Host for the Node server to listen on (default: ` + SERVER_HOST + `)
  - AWS_S3_ENDPOINT: Endpoint for the S3 storage pool server (default: ` + s3endPoint + `)
  - SMOOTHER_LATENCY: Smoother latency for hls-to-udp output (default: ` + SMOOTHER_LATENCY + `)
  - PLAYBACK_VERBOSE: Verbosity level for hls-to-udp (default: ` + PLAYBACK_VERBOSE + `)
  - RECORDING_VERBOSE: Verbosity level for udp-to-hls (default: ` + RECORDING_VERBOSE + `)
  - UDP_BUFFER_BYTES: Buffer size for hls-to-udp (default: ` + UDP_BUFFER_BYTES + `)
  - CAPTURE_BUFFER_SIZE: Buffer size for udp-to-hls (default: ` + capture_buffer_size + `)
  - MINIO_ROOT_USER: S3 Access Key (default: ` + minio_root_user + `)
  - MINIO_ROOT_PASSWORD: S3 Secret Key (default: ` + minio_root_password + `)
  - URL_SIGNING_SECONDS: S3 URL Signing time duration (default: ` + url_signing_seconds + `)
  - SEGMENT_DURATION_MS: ~Duration (estimate) of each segment in milliseconds (default: ` + segment_duration_ms + `)
  - MAX_SEGMENT_SIZE_BYTES: Maximum size of each segment in bytes (default: ` + max_segment_size_bytes + `)
  - USE_ESTIMATED_DURATION: Use estimated duration for recording (default: ` + use_estimated_duration + `)
`;
  console.log('\nRecord/Playback Server URL:', serverUrl);
  console.log('Storage Pool default S3 Endpoint:', s3endPoint);
  console.log('Swagger UI at:', serverUrl + '/api-docs');
  console.log(help_msg);
  console.log(`\nRecord/Playback Server started at: ${new Date().toISOString()}\n- Listening for connections...\n`);
});
