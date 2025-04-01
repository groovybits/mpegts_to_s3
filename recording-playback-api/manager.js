/****************************************************
 * manager.js — Recording/Playback API Manager
 * 
 * - Author: CK <ck@groovybits> https://github.com/groovybits/mpegts_to_s3
 ****************************************************/
import config from './config.js';

import express from 'express';
import { v4 as uuidv4 } from 'uuid';
import {
  PutObjectCommand,
  GetObjectCommand,
  ListObjectsV2Command,
  DeleteObjectCommand
} from '@aws-sdk/client-s3';
import fs from 'fs';
import swaggerUi from 'swagger-ui-express';
import yaml from 'js-yaml';
import fetch from 'node-fetch';

import S3Database from './S3Database.js';

const serverVersion = config.serverVersion;
const MANAGER_ID = config.MANAGER_ID;
const SERVER_PORT = config.SERVER_PORT;
const serverUrl = config.serverUrl;
const agentUrl = config.agentUrl;

const s3endPoint = config.s3endPoint;
const s3Region = config.s3Region;
const s3AccessKeyDB = config.s3AccessKeyDB;
const s3SecretKeyDB = config.s3SecretKeyDB;
const s3BucketDB = config.s3BucketDB;

// setup directorie paths and locations of files
const SWAGGER_FILE = config.MANAGER_SWAGGER_FILE;

// ----------------------------------------------------
// S3 Database initialization
// ----------------------------------------------------
console.log('Using S3 endpoint:', s3endPoint);
const db = new S3Database(s3endPoint, s3BucketDB, s3Region, s3AccessKeyDB, s3SecretKeyDB);

// ----------------------------------------------------
// Express Setup
// ----------------------------------------------------
const app = express();
app.use(express.json());

app.use('/api-docs', swaggerUi.serve, (req, res, next) => {
  try {
    const swaggerYaml = fs.readFileSync(SWAGGER_FILE, 'utf8');
    const swaggerDoc = yaml.load(swaggerYaml);

    // Dynamically set default values of the server variables
    swaggerDoc.servers[0].variables.protocol.default = req.protocol;
    swaggerDoc.servers[0].variables.host.default = req.hostname;

    // Extract port from host header
    const hostHeader = req.get('host');
    const port = hostHeader.includes(':') ? hostHeader.split(':')[1] : (req.protocol === 'https' ? '443' : '80');
    swaggerDoc.servers[0].variables.port.default = port;

    // Now serve the dynamically adjusted swaggerDoc
    swaggerUi.setup(swaggerDoc)(req, res, next);
  } catch (err) {
    console.error('Failed to load Swagger file:', err);
    res.status(500).send('Internal Server Error');
  }
});

// ===============================
// MANAGER ENDPOINTS (/v1)
// ===============================
const managerRouter = express.Router();

// --------------- RECORDINGS ---------------
managerRouter.post('/recordings', async (req, res) => {
  const { sourceUrl, duration, destinationProfile } = req.body;
  const recordingId = `rec-${uuidv4()}`;
  const now = new Date();
  const endTime = new Date(now.getTime() + duration * 1000);

  // Validate the destination profile exists
  if (destinationProfile && destinationProfile !== 'default') {
    try {
      const poolExists = await db.getPoolCredentials(destinationProfile);
      if (!poolExists) {
        return res.status(400).json({ error: `Destination profile '${destinationProfile}' not found` });
      }
    } catch (err) {
      console.error('Error validating destination profile:', err);
      return res.status(500).json({ error: 'Failed to validate destination profile' });
    }
  }

  try {
    const recordingData = {
      recordingId,
      sourceUrl,
      duration,
      destinationProfile,
      startTime: now.toISOString(),
      endTime: endTime.toISOString(),
      status: 'active',
      processPid: null
    };

    // Store in S3
    await db.s3Client.send(new PutObjectCommand({
      Bucket: db.bucket,
      Key: `recordings/${recordingId}.json`,
      Body: JSON.stringify(recordingData)
    }));

    let fetchUrl = agentUrl + "/v1/agent/jobs/recordings";
    console.log('About to call Agent with fetch, recordingId=', recordingId, ' Url=', sourceUrl, ' Duration=', duration, ' DestinationProfile=', destinationProfile, ' FetchUrl=', fetchUrl);

    try {
      const agentResp = await fetch(fetchUrl, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          jobId: recordingId,
          sourceUrl,
          duration,
          destinationProfile
        })
      });

      await agentResp.json().catch(() => ({}));

      const assetId = `asset-${uuidv4()}`;

      try {
        const assetData = {
          assetId,
          recordingId,
          destinationProfile,
          metadata: JSON.stringify(recordingData || {})
        };

        await db.s3Client.send(new PutObjectCommand({
          Bucket: db.bucket,
          Key: `assets/${assetId}.json`,
          Body: JSON.stringify(assetData)
        }));
      } catch (err) {
        console.error('Error creating asset:', err);
        res.status(500).json({ error: err.message });
      }

      return res.status(201).json({ recordingId });
    } catch (err2) {
      console.error('Error calling Agent endpoint:', err2);
      return res.status(201).json({
        recordingId,
        warning: 'Recorded in Manager S3, but Agent spawn failed. Check logs.'
      });
    }
  } catch (err) {
    console.error('Error creating recording:', err);
    return res.status(500).json({ error: err.message });
  }
});

managerRouter.get('/recordings', async (req, res) => {
  try {
    // Use the S3Database class method for pagination
    const items = await db.getS3Data('recordings/');
    res.json(items);
  } catch (err) {
    console.error('Error getting recordings:', err);
    res.status(500).json({ error: err.message });
  }
});

managerRouter.get('/recordings/:recordingId', async (req, res) => {
  const { recordingId } = req.params;

  try {
    const getParams = {
      Bucket: db.bucket,
      Key: `recordings/${recordingId}.json`
    };

    try {
      const response = await db.s3Client.send(new GetObjectCommand(getParams));
      const dataStr = await db.streamToString(response.Body);
      const data = JSON.parse(dataStr);
      res.json(data);
    } catch (err) {
      if (err.name === 'NoSuchKey') {
        return res.status(404).json({ message: 'Not found' });
      }
      throw err;
    }
  } catch (err) {
    console.error('Error getting recording:', err);
    res.status(500).json({ error: err.message });
  }
});

managerRouter.delete('/recordings/:recordingId', async (req, res) => {
  const { recordingId } = req.params;

  try {
    const getParams = {
      Bucket: db.bucket,
      Key: `recordings/${recordingId}.json`
    };

    try {
      const response = await db.s3Client.send(new GetObjectCommand(getParams));
      const dataStr = await db.streamToString(response.Body);
      const data = JSON.parse(dataStr);

      // Update status to canceled
      data.status = 'canceled';

      await db.s3Client.send(new PutObjectCommand({
        Bucket: db.bucket,
        Key: `recordings/${recordingId}.json`,
        Body: JSON.stringify(data)
      }));

      // Kill process if any
      if (data.processPid) {
        try {
          process.kill(data.processPid, 'SIGTERM');
        } catch { }
      }

      return res.sendStatus(204);
    } catch (err) {
      if (err.name === 'NoSuchKey') {
        return res.status(404).json({ message: 'Not found' });
      }
      throw err;
    }
  } catch (err) {
    console.error('Error deleting recording:', err);
    res.status(500).json({ error: err.message });
  }
});

// --------------- POOLS ---------------
managerRouter.post('/pools', async (req, res) => {
  const { bucketName, credentials } = req.body;
  if (!credentials) return res.status(400).json({ error: 'Missing credentials' });

  const poolId = `pool-${uuidv4()}`;

  try {
    const poolData = {
      poolId,
      bucketName,
      accessKey: credentials.accessKey,
      secretKey: credentials.secretKey
    };

    await db.s3Client.send(new PutObjectCommand({
      Bucket: db.bucket,
      Key: `pools/${poolId}.json`,
      Body: JSON.stringify(poolData)
    }));

    res.status(201).json({ poolId, bucketName });
  } catch (err) {
    console.error('Error creating pool:', err);
    res.status(500).json({ error: err.message });
  }
});

managerRouter.get('/pools', async (req, res) => {
  try {
    const items = await db.getS3Data('pools/');
    res.json(items);
  } catch (err) {
    console.error('Error getting pools:', err);
    res.status(500).json({ error: err.message });
  }
});

// --------------- ASSETS ---------------
managerRouter.get('/pools/:poolId/assets', async (req, res) => {
  const { poolId } = req.params;

  try {
    const items = await db.getS3Data('assets/');
    const filtered = items.filter(item => item.destinationProfile === poolId);
    res.json(filtered);
  } catch (err) {
    console.error('Error getting assets:', err);
    res.status(500).json({ error: err.message });
  }
});

managerRouter.delete('/pools/:poolId/assets', async (req, res) => {
  const { poolId } = req.params;
  const { assetId } = req.query;

  try {
    const getParams = {
      Bucket: db.bucket,
      Key: `assets/${assetId}.json`
    };

    try {
      const response = await db.s3Client.send(new GetObjectCommand(getParams));
      const dataStr = await db.streamToString(response.Body);
      const data = JSON.parse(dataStr);

      if (data.destinationProfile !== poolId) {
        return res.status(404).json({ message: 'Asset not found in this pool' });
      }

      await db.s3Client.send(new DeleteObjectCommand({
        Bucket: db.bucket,
        Key: `assets/${assetId}.json`
      }));

      res.sendStatus(204);
    } catch (err) {
      if (err.name === 'NoSuchKey') {
        return res.status(404).json({ message: 'Asset not found' });
      }
      throw err;
    }
  } catch (err) {
    console.error('Error deleting asset:', err);
    res.status(500).json({ error: err.message });
  }
});

managerRouter.get('/assets/:assetId', async (req, res) => {
  const { assetId } = req.params;

  try {
    const getParams = {
      Bucket: db.bucket,
      Key: `assets/${assetId}.json`
    };

    try {
      const response = await db.s3Client.send(new GetObjectCommand(getParams));
      const dataStr = await db.streamToString(response.Body);
      const data = JSON.parse(dataStr);

      const result = {
        assetId: data.id,
        poolId: data.poolId,
        metadata: JSON.parse(data.metadata || '{}')
      };

      res.json(result);
    } catch (err) {
      if (err.name === 'NoSuchKey') {
        return res.status(404).json({ message: 'Asset not found' });
      }
      throw err;
    }
  } catch (err) {
    console.error('Error getting asset:', err);
    res.status(500).json({ error: err.message });
  }
});

// --------------- PLAYBACKS ---------------
managerRouter.post('/playbacks', async (req, res) => {
  const { recordingId, sourceProfile, destinationUrl, duration, vodStartTime, vodEndTime } = req.body;
  const playbackId = `play-${uuidv4()}`;
  const startTime = vodStartTime;
  const endTime = vodEndTime; // TODO: improve how this works for offset to endpoint, store starttime and endtime in DB

  try {
    const playbackData = {
      playbackId,
      recordingId,
      sourceProfile,
      destinationUrl,
      duration,
      startTime: (startTime && startTime !== "") ? startTime : "",
      endTime: (endTime && endTime !== "") ? endTime : "",
      status: 'active',
      processPid: null
    };

    await db.s3Client.send(new PutObjectCommand({
      Bucket: db.bucket,
      Key: `playbacks/${playbackId}.json`,
      Body: JSON.stringify(playbackData)
    }));

    // Call the agent to start the playback
    let fetchUrl = agentUrl + "/v1/agent/jobs/playbacks";
    console.log('About to call Agent with fetch, playbackId=', playbackId, 'RecordingId=', recordingId, ' SourceProfile=', sourceProfile, ' DestinationUrl=', destinationUrl, ' Duration=', duration, ' FetchUrl=', fetchUrl);

    try {
      const agentResp = await fetch(fetchUrl, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          jobId: playbackId,
          recordingId,
          sourceProfile,
          destinationUrl,
          duration,
          vodStartTime,
          vodEndTime
        })
      });

      const agentData = await agentResp.json().catch(() => ({}));
      return res.status(201).json({ playbackId });
    } catch (err) {
      console.error('Error calling Agent endpoint:', err);
      return res.status(201).json({
        playbackId,
        warning: 'Recorded in Manager S3, but Agent spawn failed. Check logs.'
      });
    }
  } catch (err) {
    console.error('Error creating playback:', err);
    res.status(500).json({ error: err.message });
  }
});

managerRouter.get('/playbacks', async (req, res) => {
  try {
    const items = await db.getS3Data('playbacks/');
    res.json(items);
  } catch (err) {
    console.error('Error getting playbacks:', err);
    res.status(500).json({ error: err.message });
  }
});

managerRouter.get('/playbacks/:playbackId', async (req, res) => {
  const { playbackId } = req.params;

  try {
    const getParams = {
      Bucket: db.bucket,
      Key: `playbacks/${playbackId}.json`
    };

    try {
      const response = await db.s3Client.send(new GetObjectCommand(getParams));
      const dataStr = await db.streamToString(response.Body);
      const data = JSON.parse(dataStr);
      res.json(data);
    } catch (err) {
      if (err.name === 'NoSuchKey') {
        return res.status(404).json({ message: 'Playback not found' });
      }
      throw err;
    }
  } catch (err) {
    console.error('Error getting playback:', err);
    res.status(500).json({ error: err.message });
  }
});

managerRouter.delete('/playbacks/:playbackId', async (req, res) => {
  const { playbackId } = req.params;

  try {
    const getParams = {
      Bucket: db.bucket,
      Key: `playbacks/${playbackId}.json`
    };

    try {
      const response = await db.s3Client.send(new GetObjectCommand(getParams));
      const dataStr = await db.streamToString(response.Body);
      const data = JSON.parse(dataStr);

      // Update status to canceled
      data.status = 'canceled';

      await db.s3Client.send(new PutObjectCommand({
        Bucket: db.bucket,
        Key: `playbacks/${playbackId}.json`,
        Body: JSON.stringify(data)
      }));

      // Kill process if any
      if (data.processPid) {
        try {
          process.kill(data.processPid, 'SIGTERM');
        } catch { }
      }

      res.sendStatus(204);
    } catch (err) {
      if (err.name === 'NoSuchKey') {
        return res.status(404).json({ message: 'Playback not found' });
      }
      throw err;
    }
  } catch (err) {
    console.error('Error deleting playback:', err);
    res.status(500).json({ error: err.message });
  }
});

// --------------- ADMIN ---------------
managerRouter.get('/admin/stats', async (req, res) => {
  try {
    // Count active recordings
    const recItems = await db.getS3Data('recordings/');
    let concurrentRecordings = 0;
    recItems.forEach(item => {
      if (item.status === 'active') concurrentRecordings++;
    });

    // Count active playbacks
    const pbItems = await db.getS3Data('playbacks/');
    let concurrentPlaybacks = 0;
    pbItems.forEach(item => {
      if (item.status === 'active') concurrentPlaybacks++;
    });

    // Count total assets
    const asItems = await db.getS3Data('assets/');
    const totalAssets = asItems.length;

    const systemLoad = 0.2;
    const clusterUsage = { agentCount: 1 };

    res.json({
      concurrentRecordings,
      concurrentPlaybacks,
      totalAssets,
      systemLoad,
      clusterUsage
    });
  } catch (err) {
    console.error('Error getting admin stats:', err);
    res.status(500).json({ error: err.message });
  }
});

// Bind the managerRouter under /v1
app.use('/v1', managerRouter);

// ----------------------------------------------------
// Start the server
// ----------------------------------------------------
app.listen(SERVER_PORT, () => {
  console.log(`Recording / Playback Manager API Server ManagerID: [${MANAGER_ID}] Version: ${serverVersion} ManagerURL: ${serverUrl} AgentURL: ${agentUrl}`);

  // print out config.* structure neatly
  console.log('Config:', JSON.stringify(config, null, 2));

  console.log('Current Working Directory:', process.cwd());
  console.log('Storage Pool default S3 Endpoint:', s3endPoint);
  console.log('Swagger UI at:', serverUrl + '/api-docs');

  console.log(`\nRecord/Playback Manager started at: ${new Date().toISOString()}\n- Listening for connections...\n`);
});