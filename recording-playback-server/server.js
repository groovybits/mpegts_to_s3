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

// Server URL Base
const PORT = process.env.SERVER_PORT || 3000;
const serverUrl = 'http://' + (process.env.SERVER_HOST || '127.0.0.1') + ':' + PORT;
const s3endPoint = process.env.AWS_S3_ENDPOINT || 'http://192.168.50.55:9000';

// ----------------------------------------------------
// Swagger YAML read in and parsed
// ----------------------------------------------------
const swaggerYaml = fs.readFileSync('swagger.yaml', 'utf8');

const swaggerDoc = yaml.load(swaggerYaml);

// ----------------------------------------------------
// Helper: parse "udp://224.0.0.200:10001?interface=enp0s5"
// to get ip=224.0.0.200, port=10001, interface=enp0s5
// ----------------------------------------------------
function parseUdpUrl(urlString) {
  try {
    const u = new URL(urlString);
    if (u.protocol !== 'udp:') {
      return null;
    }
    const ip = u.hostname;
    const port = u.port || '10001';
    const iface = u.searchParams.get('interface') || 'enp0s5';
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
    '-v', '1',
    '-e', s3endPoint,
    '-b', s3bucketName,
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

  // Attach listeners for stdout and stderr
  child.stdout.on('data', data => {
    // strip the extra newlines
    console.log(`udp-to-hls stdout: ${data.toString().trim()}`);
  });
  child.stderr.on('data', data => {
    console.error(`udp-to-hls stderr: ${data.toString().trim()}`);
  });

  return child;
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

  // Build the vodPlaylists array by reading the ./hourly_urls.log file
  let vodPlaylists = [];
  if (vodStartTime && vodEndTime && (vodStartTime !== "" || vodEndTime !== "")) {
    if (new Date(vodEndTime) <= new Date(vodStartTime)) {
      console.log('Invalid vodEndTime, vodEndTime:', vodEndTime, ' <= vodStartTime:', vodStartTime);
      return [];
    }
    if (!new Date(vodStartTime).toISOString().includes(vodStartTime)) {
      console.log('Invalid vodStartTime, vodStartTime:', vodStartTime);
      return [];
    }
    if (!new Date(vodEndTime).toISOString().includes(vodEndTime)) {
      console.log('Invalid vodEndTime, vodEndTime:', vodEndTime);
      return [];
    }
    const lines = fs.readFileSync('./hourly_urls.log', 'utf-8').split('\n');
    for (const line of lines) {
      if (!line) continue;
      if (!line.startsWith('Hour')) continue;
      const [hour, url] = line.split(' => ');
      if (!hour || !url) {
        console.log('Invalid hour, url line:', line);
        continue;
      }
      const [jobId_prefix, year, month, day, hourStr] = hour.split('/');
      const [_, newJobId] = jobId_prefix.split(' ');
      if (!hourStr) {
        console.log('Invalid date, hourStr:', hourStr);
        continue;
      }
      if (newJobId !== jobId) continue;
      const hourDate = new Date(`${year}-${month}-${day}T${hourStr}:00:00Z`);
      if (hourDate >= new Date(vodStartTime) && hourDate <= new Date(vodEndTime)) {
        console.log('Using hls playback url:', url);
        vodPlaylists.push(url);
      }
    }
  } else {
    // No start/end times; get all URLs matching the jobId from the playlist file.
    const lines = fs.readFileSync('./hourly_urls.log', 'utf-8').split('\n');
    for (const line of lines) {
      if (!line) continue;
      if (line.startsWith('Hour')) {
        const [hour, url] = line.split(' => ');
        if (!hour || !url) {
          console.log('Invalid hour, url line:', line);
          continue;
        }
        const [jobId_prefix, year, month, day, hourStr] = hour.split('/');
        const [_, newJobId] = jobId_prefix.split(' ');
        if (!hourStr) {
          console.log('Invalid date, hourStr:', hourStr);
          continue;
        }
        if (newJobId !== jobId) continue;
        if (url) {
          vodPlaylists.push(url);
          console.log('Using hls playback url:', url);
        }
      }
    }
  }

  let childArray = [];
  // Spawn each hls-to-udp process.
  // Wait only for the first one if there is more than one playlist.
  for (let i = 0; i < vodPlaylists.length; i++) {
    let baseArgs = [];
    baseArgs.push('--vod');
    baseArgs.push('--use-smoother');
    baseArgs.push('-v', '1');
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
          console.log(`Auto-stopping recording jobId=${jobId} after duration`);
          // Stop the process
          try { process.kill(pid, 'SIGTERM'); } catch { }
          db.run(`DELETE FROM agent_recordings WHERE jobId=?`, [jobId]);
          activeTimers.recordings.delete(jobId);
        }, (duration + 10) * 1000); // Add 10s buffer
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
          }, (duration) * 1000);
          activeTimers.playbacks.set(jobId, timer);
        } else {
          /* get the duration from the db */
          db.get(`SELECT duration FROM agent_playbacks WHERE jobId=?`, [jobId], (err, row) => {
            if (err) return res.status(500).json({ error: err.message });
            if (!row) return res.status(404).json({ message: 'Not found' });
            const durationFull = row.duration;
            if (durationFull && durationFull > 0) {
              const timer = setTimeout(() => {
                console.log(`Auto-stopping playback jobId=${jobId} after duration ${durationFull}`);
                try { process.kill(pid, 'SIGTERM'); } catch { }
                db.run(`DELETE FROM agent_playbacks WHERE jobId=?`, [jobId]);
                activeTimers.playbacks.delete(jobId);
              }, (durationFull + 10) * 1000);
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
app.listen(PORT, () => {
  console.log(`Server running on ${serverUrl}. Swagger at ${serverUrl}/api-docs`);
});
