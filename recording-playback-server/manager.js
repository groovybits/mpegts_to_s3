/****************************************************
 * manager.js — Recording/Playback API Manager
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
 * - The server will store the job status in S3 as JSON files
 * - The server will auto-stop the jobs after the given durations
 * - The server will store the recording URLs in S3
 * - The server will serve the recording URLs to the clients
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
 * - Run `node server.js` to start the server
 * 
 * - Author: CK <ck@groovybits> https://github.com/groovybits/mpegts_to_s3
 * 
 ****************************************************/

const serverVersion = '1.1.1';

const express = require('express');
const { v4: uuidv4 } = require('uuid');
const { S3Client, PutObjectCommand, GetObjectCommand, ListObjectsV2Command, DeleteObjectCommand } = require('@aws-sdk/client-s3');
const fs = require('fs');

// For Swagger
const swaggerUi = require('swagger-ui-express');
const yaml = require('js-yaml');

// For fetch
const fetch = require('node-fetch');
const { env } = require('process');

/*
 * The SERVER_HOST and SERVER_PORT are the host and port of the recording-playback-server which can be a Manager or Agent role.
 * The AGENT_HOST and AGENT_PORT are the host and port of the recording-playback-server which can be remotely called by a Manager role.
 * Agents are basically nodes that run the heavier processes of recording and playback.
 * Managers are nodes that manage the Agents and the recording and playback processes.
 */

// Server Manager and Agent URL Bases used for fetch calls (same server in this case)
const SERVER_PROTOCOL = process.env.SERVER_PROTOCOL || 'http';
const SERVER_PORT = process.env.SERVER_PORT || 3000;
const SERVER_HOST = process.env.SERVER_HOST || "127.0.0.1"; // Manager and local Agents base server
const serverUrl = SERVER_PROTOCOL + '://' + SERVER_HOST + ':' + SERVER_PORT;

const AGENT_PROTOCOL = process.env.AGENT_PROTOCOL || 'http';
const AGENT_PORT = process.env.AGENT_PORT || 3000;
const AGENT_HOST = process.env.AGENT_HOST || "127.0.0.1"; // Agent is running on the same server as Manager (us)
const agentUrl = AGENT_PROTOCOL + '://' + AGENT_HOST + ':' + AGENT_PORT;

// S3 endpoint for the MinIO server
const s3endPoint = process.env.AWS_S3_ENDPOINT || 'http://127.0.0.1:9000';
const s3Region = process.env.AWS_REGION || 'us-east-1';
const s3AccessKeyDB = process.env.AWS_ACCESS_KEY_ID || 'minioadmin';
const s3SecretKeyDB = process.env.AWS_SECRET_ACCESS_KEY || 'minioadmin';
const s3BucketDB = process.env.AWS_S3_BUCKET || 'hls';

// setup directorie paths and locations of files
const SWAGGER_FILE = process.env.SWAGGER_FILE || 'swagger_manager.yaml';

/**
 * S3Database - A class to handle database operations using S3 and JSON files
 * Each table is stored as a collection of JSON files in an S3 bucket
 */
class S3Database {
  constructor(endpoint, bucket, region = 'us-east-1', accessKey = null, secretKey = null) {
    // Create S3 client
    this.s3Client = new S3Client({
      region,
      endpoint,
      credentials: accessKey && secretKey ? {
        accessKeyId: accessKey,
        secretAccessKey: secretKey
      } : undefined,
      forcePathStyle: true // Needed for MinIO and other S3-compatible servers
    });

    // Default bucket name
    this.bucket = bucket;

    // Track tables that have been initialized
    this.tables = new Set();
  }

  /**
   * Ensure the table exists in S3
   * @param {string} tableName - Table name
   */
  async ensureTable(tableName) {
    if (this.tables.has(tableName)) return;

    try {
      const params = {
        Bucket: this.bucket,
        Key: `${tableName}/_metadata.json`
      };

      try {
        await this.s3Client.send(new GetObjectCommand(params));
      } catch (err) {
        // If metadata doesn't exist, create it
        const createParams = {
          Bucket: this.bucket,
          Key: `${tableName}/_metadata.json`,
          Body: JSON.stringify({
            tableName,
            created: new Date().toISOString()
          })
        };

        await this.s3Client.send(new PutObjectCommand(createParams));
      }

      this.tables.add(tableName);
    } catch (err) {
      console.error(`Error ensuring table ${tableName}:`, err);
      throw err;
    }
  }

  /**
   * Get a single record by ID
   * @param {string} sql - SQL-like SELECT statement (parsed for table name and WHERE clause)
   * @param {array} params - Parameters (usually just the ID)
   * @param {function} callback - Callback function(err, row)
   */
  async get(sql, params, callback) {
    try {
      // Extract table name from SELECT statement
      const tableMatch = sql.match(/FROM\s+(\w+)/i);
      if (!tableMatch || !tableMatch[1]) {
        throw new Error('Could not parse table name from SQL');
      }

      const tableName = tableMatch[1];
      await this.ensureTable(tableName);

      // Extract ID field and value from WHERE clause
      const whereMatch = sql.match(/WHERE\s+(\w+)\s*=\s*\?/i);
      if (!whereMatch || !whereMatch[1]) {
        throw new Error('Could not parse ID field from WHERE clause');
      }

      const idField = whereMatch[1];
      const id = params[0];

      // Get the item from S3
      const getParams = {
        Bucket: this.bucket,
        Key: `${tableName}/${id}.json`
      };

      try {
        const response = await this.s3Client.send(new GetObjectCommand(getParams));
        const dataStr = await streamToString(response.Body);
        const data = JSON.parse(dataStr);

        if (callback) callback(null, data);
        return data;
      } catch (err) {
        if (err.name === 'NoSuchKey') {
          // Item not found
          if (callback) callback(null, null);
          return null;
        }
        throw err;
      }
    } catch (err) {
      console.error('Error in get operation:', err);
      if (callback) callback(err, null);
      return null;
    }
  }

  /**
   * Get multiple records, optionally filtered
   * @param {string} sql - SQL-like SELECT statement
   * @param {array} params - Parameters for filtering
   * @param {function} callback - Callback function(err, rows)
   */
  async all(sql, params = [], callback) {
    try {
      // Extract table name from SELECT statement
      const tableMatch = sql.match(/FROM\s+(\w+)/i);
      if (!tableMatch || !tableMatch[1]) {
        throw new Error('Could not parse table name from SQL');
      }

      const tableName = tableMatch[1];
      await this.ensureTable(tableName);

      // List all objects for this table
      const listParams = {
        Bucket: this.bucket,
        Prefix: `${tableName}/`,
        MaxKeys: 1000 // Adjust as needed
      };

      const response = await this.s3Client.send(new ListObjectsV2Command(listParams));
      const items = [];

      if (response.Contents) {
        // Extract WHERE clause if present
        let whereClause = null;
        let whereField = null;
        let whereValue = null;

        if (sql.includes('WHERE') && params.length > 0) {
          const whereMatch = sql.match(/WHERE\s+(\w+)\s*=\s*\?/i);
          if (whereMatch && whereMatch[1]) {
            whereField = whereMatch[1];
            whereValue = params[0];
          }
        }

        // Fetch and filter items
        for (const obj of response.Contents) {
          const key = obj.Key;
          if (key.endsWith('_metadata.json')) continue;

          const getParams = {
            Bucket: this.bucket,
            Key: key
          };

          try {
            const itemResponse = await this.s3Client.send(new GetObjectCommand(getParams));
            const dataStr = await streamToString(itemResponse.Body);
            const data = JSON.parse(dataStr);

            // Apply WHERE filter if needed
            if (whereField && whereValue !== null) {
              if (data[whereField] === whereValue) {
                items.push(data);
              }
            } else {
              items.push(data);
            }
          } catch (err) {
            console.error(`Error reading ${key}:`, err);
          }
        }
      }

      if (callback) callback(null, items);
      return items;
    } catch (err) {
      console.error('Error in all operation:', err);
      if (callback) callback(err, []);
      return [];
    }
  }
}

// Helper to convert a stream to a string
async function streamToString(stream) {
  const chunks = [];
  return new Promise((resolve, reject) => {
    stream.on('data', (chunk) => chunks.push(Buffer.from(chunk)));
    stream.on('error', (err) => reject(err));
    stream.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
  });
}

/**
 * Looks up pool credentials by profile name/ID
 * @param {string} profileId - The pool ID or profile name to look up
 * @returns {Promise<{bucketName: string, accessKey: string, secretKey: string}|null>}
 */
async function getPoolCredentials(profileId) {
  if (!profileId || profileId === 'default') {
    // Return default credentials
    return {
      bucketName: s3BucketDB,
      accessKey: s3AccessKeyDB,
      secretKey: s3SecretKeyDB
    };
  }

  try {
    // Lookup the pool in S3
    const getParams = {
      Bucket: db.bucket,
      Key: `pools/${profileId}.json`
    };

    console.log('getPoolCredentials: Looking up pool:', profileId);

    try {
      const response = await db.s3Client.send(new GetObjectCommand(getParams));
      const dataStr = await streamToString(response.Body);
      const poolData = JSON.parse(dataStr);

      if (!poolData || !poolData.bucketName || !poolData.accessKey || !poolData.secretKey) {
        if (poolData) {
          console.error(`getPoolCredentials: Pool ${profileId} has missing credentials:`, poolData);
        } else {
          if (!poolData.bucketName) console.error(`getPoolCredentials: Pool ${profileId} missing bucketName`);
          if (!poolData.accessKey) console.error(`getPoolCredentials: Pool ${profileId} missing accessKey`);
          if (!poolData.secretKey) console.error(`getPoolCredentials: Pool ${profileId} missing secretKey`);
        }
        return null;
      }

      return {
        bucketName: poolData.bucketName || s3BucketDB,
        accessKey: poolData.accessKey || s3AccessKeyDB,
        secretKey: poolData.secretKey || s3SecretKeyDB
      };
    } catch (err) {
      if (err.name === 'NoSuchKey') {
        console.error(`getPoolCredentials: Error with Pool ${profileId} not found with error:`, err.name, err.message);
        return null;
      }
      throw err;
    }
  } catch (err) {
    console.error(`getPoolCredentials: Error getting pool credentials for ${profileId}:`, err);
    return null;
  }
}

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
      const poolExists = await getPoolCredentials(destinationProfile);
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

      const agentData = await agentResp.json().catch(() => ({}));
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
    const listParams = {
      Bucket: db.bucket,
      Prefix: 'recordings/',
      MaxKeys: 1000
    };

    const response = await db.s3Client.send(new ListObjectsV2Command(listParams));
    const items = [];

    if (response.Contents) {
      for (const obj of response.Contents) {
        const getParams = {
          Bucket: db.bucket,
          Key: obj.Key
        };

        try {
          const itemResponse = await db.s3Client.send(new GetObjectCommand(getParams));
          const dataStr = await streamToString(itemResponse.Body);
          const data = JSON.parse(dataStr);
          items.push(data);
        } catch (err) {
          console.error(`Error reading ${obj.Key}:`, err);
        }
      }
    }

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
      const dataStr = await streamToString(response.Body);
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
      const dataStr = await streamToString(response.Body);
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
    const listParams = {
      Bucket: db.bucket,
      Prefix: 'pools/',
      MaxKeys: 1000
    };

    const response = await db.s3Client.send(new ListObjectsV2Command(listParams));
    const items = [];

    if (response.Contents) {
      for (const obj of response.Contents) {
        const getParams = {
          Bucket: db.bucket,
          Key: obj.Key
        };

        try {
          const itemResponse = await db.s3Client.send(new GetObjectCommand(getParams));
          const dataStr = await streamToString(itemResponse.Body);
          const data = JSON.parse(dataStr);
          items.push(data);
        } catch (err) {
          console.error(`Error reading ${obj.Key}:`, err);
        }
      }
    }

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
    const listParams = {
      Bucket: db.bucket,
      Prefix: 'assets/',
      MaxKeys: 1000
    };

    const response = await db.s3Client.send(new ListObjectsV2Command(listParams));
    const items = [];

    if (response.Contents) {
      for (const obj of response.Contents) {
        const getParams = {
          Bucket: db.bucket,
          Key: obj.Key
        };

        try {
          const itemResponse = await db.s3Client.send(new GetObjectCommand(getParams));
          const dataStr = await streamToString(itemResponse.Body);
          const data = JSON.parse(dataStr);

          if (data.poolId === poolId) {
            items.push(data);
          }
        } catch (err) {
          console.error(`Error reading ${obj.Key}:`, err);
        }
      }
    }

    res.json(items);
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
      const dataStr = await streamToString(response.Body);
      const data = JSON.parse(dataStr);

      if (data.poolId !== poolId) {
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
      const dataStr = await streamToString(response.Body);
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
    const listParams = {
      Bucket: db.bucket,
      Prefix: 'playbacks/',
      MaxKeys: 1000
    };

    const response = await db.s3Client.send(new ListObjectsV2Command(listParams));
    const items = [];

    if (response.Contents) {
      for (const obj of response.Contents) {
        const getParams = {
          Bucket: db.bucket,
          Key: obj.Key
        };

        try {
          const itemResponse = await db.s3Client.send(new GetObjectCommand(getParams));
          const dataStr = await streamToString(itemResponse.Body);
          const data = JSON.parse(dataStr);
          items.push(data);
        } catch (err) {
          console.error(`Error reading ${obj.Key}:`, err);
        }
      }
    }

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
      const dataStr = await streamToString(response.Body);
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
      const dataStr = await streamToString(response.Body);
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
    const recListParams = {
      Bucket: db.bucket,
      Prefix: 'recordings/',
      MaxKeys: 1000
    };

    const recResponse = await db.s3Client.send(new ListObjectsV2Command(recListParams));
    let concurrentRecordings = 0;

    if (recResponse.Contents) {
      for (const obj of recResponse.Contents) {
        const getParams = {
          Bucket: db.bucket,
          Key: obj.Key
        };

        try {
          const itemResponse = await db.s3Client.send(new GetObjectCommand(getParams));
          const dataStr = await streamToString(itemResponse.Body);
          const data = JSON.parse(dataStr);

          if (data.status === 'active') {
            concurrentRecordings++;
          }
        } catch (err) {
          console.error(`Error reading ${obj.Key}:`, err);
        }
      }
    }

    // Count active playbacks
    const pbListParams = {
      Bucket: db.bucket,
      Prefix: 'playbacks/',
      MaxKeys: 1000
    };

    const pbResponse = await db.s3Client.send(new ListObjectsV2Command(pbListParams));
    let concurrentPlaybacks = 0;

    if (pbResponse.Contents) {
      for (const obj of pbResponse.Contents) {
        const getParams = {
          Bucket: db.bucket,
          Key: obj.Key
        };

        try {
          const itemResponse = await db.s3Client.send(new GetObjectCommand(getParams));
          const dataStr = await streamToString(itemResponse.Body);
          const data = JSON.parse(dataStr);

          if (data.status === 'active') {
            concurrentPlaybacks++;
          }
        } catch (err) {
          console.error(`Error reading ${obj.Key}:`, err);
        }
      }
    }

    // Count total assets
    const asListParams = {
      Bucket: db.bucket,
      Prefix: 'assets/',
      MaxKeys: 1000
    };

    const asResponse = await db.s3Client.send(new ListObjectsV2Command(asListParams));
    const totalAssets = asResponse.Contents ? asResponse.Contents.length : 0;

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
  console.log(`Manager/Agent API Server version ${serverVersion} Manager@${serverUrl} AgentUrl@${agentUrl}.`);
  let minio_root_user = env.MINIO_ROOT_USER || `minioadmin`;
  let minio_root_password = env.MINIO_ROOT_PASSWORD || `minioadmin`;

  const help_msg = `
Environment Variables:
  - SERVER_PORT: Port for the Node server to listen on as a Manager or Agent (default: ` + SERVER_PORT + `)
  - SERVER_HOST: Host for the Node server to listen on as a Manager or Agent (default: ` + SERVER_HOST + `)
  - AGENT_PORT: Port for the Agent server used by the Manager (default: ` + AGENT_PORT + `)
  - AGENT_HOST: Host for the Agent server used by the Manager (default: ` + AGENT_HOST + `)
  - AWS_S3_ENDPOINT: Default Endpoint for the S3 storage pool server (default: ` + s3endPoint + `)
  - AWS_S3_REGION: Default Region for the S3 storage pool server (default: ` + s3Region + `)
  - MINIO_ROOT_USER: Default S3 Access Key (default: ` + minio_root_user + `)
  - MINIO_ROOT_PASSWORD: Default S3 Secret Key (default: ` + minio_root_password + `)
  - SWAGGER_FILE: Path to the Swagger file (default: ` + SWAGGER_FILE + `)
`;
  console.log('\nRecord/Playback API Manager URL:', serverUrl, ' Agent URL:', agentUrl);
  console.log('Current Working Directory:', process.cwd());
  console.log('Storage Pool default S3 Endpoint:', s3endPoint);
  console.log('Swagger UI at:', serverUrl + '/api-docs');
  console.log(help_msg);

  console.log(`\nRecord/Playback Manager started at: ${new Date().toISOString()}\n- Listening for connections...\n`);
});
