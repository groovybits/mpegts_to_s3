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

const serverVersion = '1.0.30';

const express = require('express');
const { v4: uuidv4 } = require('uuid');
const { spawn } = require('child_process');
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
const s3AccessKey = process.env.AWS_ACCESS_KEY_ID || 'minioadmin';
const s3SecretKey = process.env.AWS_SECRET_ACCESS_KEY || 'minioadmin';
const s3Bucket = process.env.AWS_S3_BUCKET || 'hls';

// Runtime verbosity levels of Rust programs, 0-4: error, warn, info, debug, trace
const PLAYBACK_VERBOSE = process.env.PLAYBACK_VERBOSE || 2;
const RECORDING_VERBOSE = process.env.RECORDING_VERBOSE || 2;

// check env and set the values for the baseargs, else set to defaults, use vars below
const SMOOTHER_LATENCY = process.env.SMOOTHER_LATENCY || 500;
const UDP_BUFFER_BYTES = process.env.UDP_BUFFER_BYTES || 1316;

// setup directorie paths and locations of files
const SWAGGER_FILE = process.env.SWAGGER_FILE || 'swagger.yaml';
const ORIGINAL_DIR = process.cwd() + '/';
const HLS_DIR = process.env.HLS_DIR || '';

// Add ../bin/ to PATH env variable if it exists
if (fs.existsSync('../bin/')) {
  process.env.PATH = process.env.PATH + ':../bin';
}

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
   * Initialize table structure if needed (similar to CREATE TABLE IF NOT EXISTS)
   * @param {string} tableName - Name of the table
   * @param {object} schema - Optional schema definition
   */
  async serialize(callback) {
    // Initialize standard tables
    await this.run(`
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

    await this.run(`
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

    await this.run(`
      CREATE TABLE IF NOT EXISTS pools (
        id TEXT PRIMARY KEY,
        bucketName TEXT,
        accessKey TEXT,
        secretKey TEXT
      )
    `);

    await this.run(`
      CREATE TABLE IF NOT EXISTS assets (
        id TEXT PRIMARY KEY,
        poolId TEXT,
        metadata TEXT
      )
    `);

    await this.run(`
      CREATE TABLE IF NOT EXISTS recording_urls (
        jobId TEXT,
        hour TEXT,
        url TEXT,
        PRIMARY KEY (jobId, hour)
      )
    `);

    await this.run(`
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

    await this.run(`
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

    if (callback) {
      callback();
    }
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
   * Execute a "CREATE TABLE" statement (creates the table metadata in S3)
   * @param {string} sql - SQL-like CREATE TABLE statement (parsed for table name only)
   */
  async run(sql, params = [], callback) {
    try {
      // Extract table name from CREATE TABLE statement
      if (typeof sql === 'string' && sql.trim().toUpperCase().startsWith('CREATE TABLE')) {
        const match = sql.match(/CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?(\w+)/i);
        if (match && match[1]) {
          const tableName = match[1];
          await this.ensureTable(tableName);
          if (callback) callback();
          return;
        }
      }

      // For non-CREATE TABLE statements, handle INSERT, UPDATE, DELETE
      if (typeof sql === 'string') {
        // INSERT
        if (sql.trim().toUpperCase().startsWith('INSERT INTO')) {
          const match = sql.match(/INSERT\s+INTO\s+(\w+)/i);
          if (match && match[1]) {
            const tableName = match[1];
            await this.ensureTable(tableName);

            // For simplicity, assume params contains the full object to insert
            const id = params[0]; // Assuming first param is ID
            const data = {};

            // Parse the columns and values from the SQL
            const colMatch = sql.match(/\(([^)]+)\)/);
            if (colMatch && colMatch[1]) {
              const columns = colMatch[1].split(',').map(c => c.trim());

              // Combine columns with params to create data object
              for (let i = 0; i < columns.length; i++) {
                data[columns[i]] = params[i];
              }

              // Store in S3
              const putParams = {
                Bucket: this.bucket,
                Key: `${tableName}/${id}.json`,
                Body: JSON.stringify(data)
              };

              await this.s3Client.send(new PutObjectCommand(putParams));
              if (callback) callback();
              return;
            }
          }
        }

        // UPDATE
        if (sql.trim().toUpperCase().startsWith('UPDATE')) {
          const match = sql.match(/UPDATE\s+(\w+)\s+SET/i);
          if (match && match[1]) {
            const tableName = match[1];
            await this.ensureTable(tableName);

            // Extract ID from WHERE clause (e.g., "WHERE id=?")
            const whereMatch = sql.match(/WHERE\s+(\w+)\s*=\s*\?/i);
            if (whereMatch && whereMatch[1]) {
              const idField = whereMatch[1];
              const id = params[params.length - 1]; // Assuming ID is the last param

              // Get existing item
              const getParams = {
                Bucket: this.bucket,
                Key: `${tableName}/${id}.json`
              };

              try {
                const response = await this.s3Client.send(new GetObjectCommand(getParams));
                const dataStr = await streamToString(response.Body);
                const existingData = JSON.parse(dataStr);

                // Parse SET clause to determine updates
                const setMatch = sql.match(/SET\s+([^WHERE]+)/i);
                if (setMatch && setMatch[1]) {
                  const setParts = setMatch[1].split(',').map(p => p.trim());
                  const updates = {};

                  for (let i = 0; i < setParts.length; i++) {
                    const fieldMatch = setParts[i].match(/(\w+)\s*=\s*\?/);
                    if (fieldMatch && fieldMatch[1]) {
                      const field = fieldMatch[1];
                      updates[field] = params[i];
                    }
                  }

                  // Merge updates with existing data
                  const updatedData = { ...existingData, ...updates };

                  // Store updated data
                  const putParams = {
                    Bucket: this.bucket,
                    Key: `${tableName}/${id}.json`,
                    Body: JSON.stringify(updatedData)
                  };

                  await this.s3Client.send(new PutObjectCommand(putParams));
                  if (callback) callback();
                  return;
                }
              } catch (err) {
                console.error(`Error updating item in ${tableName}:`, err);
                if (callback) callback(err);
                return;
              }
            }
          }
        }

        // DELETE
        if (sql.trim().toUpperCase().startsWith('DELETE FROM')) {
          const match = sql.match(/DELETE\s+FROM\s+(\w+)/i);
          if (match && match[1]) {
            const tableName = match[1];
            await this.ensureTable(tableName);

            // Extract ID from WHERE clause
            const whereMatch = sql.match(/WHERE\s+(\w+)\s*=\s*\?/i);
            if (whereMatch && whereMatch[1]) {
              const idField = whereMatch[1];
              const id = params[0]; // Assuming ID is the first param

              // Delete from S3
              const deleteParams = {
                Bucket: this.bucket,
                Key: `${tableName}/${id}.json`
              };

              try {
                await this.s3Client.send(new DeleteObjectCommand(deleteParams));
                if (callback) callback();
                return;
              } catch (err) {
                console.error(`Error deleting from ${tableName}:`, err);
                if (callback) callback(err);
                return;
              }
            }
          }
        }
      }

      // Handle direct object inserts (for simplified API)
      if (typeof sql === 'object' && params && params.length >= 2) {
        const tableName = sql.table;
        const data = sql.data;
        const id = data.id || uuidv4();

        // Ensure ID is set
        data.id = id;

        await this.ensureTable(tableName);

        // Store in S3
        const putParams = {
          Bucket: this.bucket,
          Key: `${tableName}/${id}.json`,
          Body: JSON.stringify(data)
        };

        await this.s3Client.send(new PutObjectCommand(putParams));
        if (callback) callback(null, { id });
        return;
      }

      // If we get here, the operation wasn't recognized
      console.error('Unrecognized operation:', sql);
      if (callback) callback(new Error('Unrecognized operation'));
    } catch (err) {
      console.error('Error in run operation:', err);
      if (callback) callback(err);
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
 * Helper to read index.txt and store recording URLs into S3 for a given jobId.
 */
async function storeRecordingUrls(jobId) {
  // Read from the index.txt file, as in the original implementation
  const hourly_urls = ORIGINAL_DIR + HLS_DIR + 'index.txt';

  /* check if hourly_urls file exists */
  if (!fs.existsSync(hourly_urls)) {
    console.error('Hourly URLs file does not exist:', hourly_urls, ' for jobId:', jobId, ' current working directory:', process.cwd());
    return;
  }

  try {
    const hourlyUrlsContent = fs.readFileSync(hourly_urls, 'utf-8');

    const lines = hourlyUrlsContent.split('\n');
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

      // Store recording URL in S3
      const recordingUrlData = {
        jobId,
        hour: hourString,
        url: url.trim()
      };

      const key = `recording_urls/${jobId}_${hourString.replace(/\//g, '_')}.json`;

      try {
        await db.s3Client.send(new PutObjectCommand({
          Bucket: db.bucket,
          Key: key,
          Body: JSON.stringify(recordingUrlData)
        }));

        console.log('Inserted recording url', url.trim(), 'into S3 for job:', jobId, 'hour:', hourString);
      } catch (err) {
        console.error('Error inserting recording url into S3 for job', jobId, ':', err);
      }
    }
  } catch (err) {
    console.error('Error reading hourly URLs file:', hourly_urls, err);
  }
}

// ----------------------------------------------------
// S3 Database initialization
// ----------------------------------------------------
console.log('Using S3 endpoint:', s3endPoint);
const db = new S3Database(s3endPoint, s3Bucket, s3Region, s3AccessKey, s3SecretKey);
db.serialize(() => {
  console.log('Initializing S3 database tables...');
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
managerRouter.post('/recordings', async (req, res) => {
  const { sourceUrl, duration, destinationProfile } = req.body;
  const recordingId = `rec-${uuidv4()}`;
  const now = new Date();
  const endTime = new Date(now.getTime() + duration * 1000);

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
  const { sourceProfile, destinationUrl, duration, vodStartTime, vodEndTime } = req.body;
  const playbackId = `play-${uuidv4()}`;
  const startTime = vodStartTime;
  const endTime = vodEndTime; // TODO: improve how this works for offset to endpoint, store starttime and endtime in DB

  try {
    const playbackData = {
      playbackId,
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
    console.log('About to call Agent with fetch, playbackId=', playbackId, ' SourceProfile=', sourceProfile, ' DestinationUrl=', destinationUrl, ' Duration=', duration, ' FetchUrl=', fetchUrl);

    try {
      const agentResp = await fetch(fetchUrl, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          jobId: sourceProfile,
          playbackId,
          destinationUrl,
          duration
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
    // For now, we'll just throw
    throw new Error(`Invalid or non-UDP sourceUrl: ${sourceUrl}`);
  }

  const { ip, port, iface } = parsed;
  // Example invocation, adjust arguments as needed.
  // We'll store segments in a subdirectory named by jobId.
  const args = [
    '-n', iface,
    '-v', `${RECORDING_VERBOSE}`,
    '-e', s3endPoint,
    '-b', s3bucketName,
    //'--duration', duration,
    '--hls_keep_segments', '0', // Keep all segments
    '-i', ip,
    '-p', port,
    '-o', jobId  // acts as the "channel name"/local output folder
  ];

  console.log(`Spawning => udp-to-hls ${args.join(' ')}`);
  const child = spawn('udp-to-hls', args, {
    stdio: ['ignore', 'pipe', 'pipe']
  });

  // Attach listeners for stdout and stderr
  child.stdout.on('data', data => {
    const output = data.toString().trim();
    console.log(`udp-to-hls stdout: ${output}`);
    // If output starts with "Hour", parse and store in S3
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

          const recordingUrlData = {
            jobId: currentJobId,
            hour: hourString,
            url: url.trim()
          };

          // Generate a unique key for S3
          const key = `recording_urls/${currentJobId}_${hourString.replace(/\//g, '_')}.json`;

          // Store in S3
          db.s3Client.send(new PutObjectCommand({
            Bucket: db.bucket,
            Key: key,
            Body: JSON.stringify(recordingUrlData)
          })).then(() => {
            console.log('Inserted recording url into S3 for job:', currentJobId, 'hour:', hourString);
          }).catch(err => {
            console.error('Error inserting recording url into S3:', err);
          });
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
  try {
    const listParams = {
      Bucket: db.bucket,
      Prefix: 'recording_urls/',
      MaxKeys: 1000
    };

    const response = await db.s3Client.send(new ListObjectsV2Command(listParams));
    const vodPlaylists = [];

    if (response.Contents) {
      for (const obj of response.Contents) {
        const key = obj.Key;
        if (!key.startsWith(`recording_urls/${jobId}_`)) continue;

        const getParams = {
          Bucket: db.bucket,
          Key: key
        };

        try {
          const itemResponse = await db.s3Client.send(new GetObjectCommand(getParams));
          const dataStr = await streamToString(itemResponse.Body);
          const data = JSON.parse(dataStr);

          // data.hour is expected in format "yyyy/mm/dd/hh:mm:ss"
          const dateParts = data.hour.split('/');
          if (dateParts.length < 4) {
            console.log('Invalid date format in recording_urls:', data.hour);
            continue;
          }

          const isoString = `${dateParts[0]}-${dateParts[1]}-${dateParts[2]}T${dateParts[3]}Z`;
          const hourDate = new Date(isoString);

          if (vodStartTime && vodEndTime && (vodStartTime !== "" || vodEndTime !== "")) {
            if (hourDate >= new Date(vodStartTime) && hourDate <= new Date(vodEndTime)) {
              console.log('Using hls playback url:', data.url);
              vodPlaylists.push(data.url);
            }
          } else {
            console.log('Using hls playback url:', data.url);
            vodPlaylists.push(data.url);
          }
        } catch (err) {
          console.error(`Error reading ${key}:`, err);
        }
      }
    }

    return vodPlaylists;
  } catch (err) {
    console.error('Error getting vod playlists from S3:', err);
    return [];
  }
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

  // Build the vodPlaylists array by querying the recording_urls in S3
  let vodPlaylists = [];
  try {
    vodPlaylists = await getVodPlaylists(jobId, vodStartTime, vodEndTime);
  } catch (err) {
    console.error('Error getting vod playlists from S3:', err);
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

    console.log(`Spawning hls-to-udp => ${baseArgs.join(' ')}`);
    const child = spawn('hls-to-udp', baseArgs, {
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
agentRouter.get('/status', async (req, res) => {
  try {
    // Get all agent recordings
    const recListParams = {
      Bucket: db.bucket,
      Prefix: 'agent_recordings/',
      MaxKeys: 1000
    };

    const recResponse = await db.s3Client.send(new ListObjectsV2Command(recListParams));
    const recRows = [];

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
          recRows.push(data);
        } catch (err) {
          console.error(`Error reading ${obj.Key}:`, err);
        }
      }
    }

    // Get all agent playbacks
    const pbListParams = {
      Bucket: db.bucket,
      Prefix: 'agent_playbacks/',
      MaxKeys: 1000
    };

    const pbResponse = await db.s3Client.send(new ListObjectsV2Command(pbListParams));
    const pbRows = [];

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
          pbRows.push(data);
        } catch (err) {
          console.error(`Error reading ${obj.Key}:`, err);
        }
      }
    }

    res.json({
      agentId: 'agent-001',
      status: 'healthy',
      activeRecordings: recRows,
      activePlaybacks: pbRows
    });
  } catch (err) {
    console.error('Error getting agent status:', err);
    res.status(500).json({ error: err.message });
  }
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

  // Store in S3
  const recordingData = {
    jobId,
    sourceUrl,
    duration,
    destinationProfile,
    status: 'running',
    processPid: pid,
    startTime: now.toISOString()
  };

  db.s3Client.send(new PutObjectCommand({
    Bucket: db.bucket,
    Key: `agent_recordings/${jobId}.json`,
    Body: JSON.stringify(recordingData)
  })).then(() => {
    // If duration > 0, auto-stop after duration
    if (duration && duration > 0) {
      let max_duration = duration * 1.2;
      const timer = setTimeout(async () => {
        console.log(`Auto-stopping recording jobId=${jobId} of duration=${duration} after max_duration=${max_duration}`);
        // Stop the process
        try {
          process.kill(pid, 'SIGTERM');
          console.log(`Killed recording jobId=${jobId} of duration ${duration}`);
        } catch (err) {
          console.error(`Error killing recording jobId=${jobId} of duration ${duration}: ${err}`);
        }

        // Delete from S3
        try {
          await db.s3Client.send(new DeleteObjectCommand({
            Bucket: db.bucket,
            Key: `agent_recordings/${jobId}.json`
          }));
        } catch (err) {
          console.error(`Error deleting recording from S3:`, err);
        }

        activeTimers.recordings.delete(jobId);
      }, (max_duration) * 1000); // Add up to 20% buffer for GOP alignment or other delays

      activeTimers.recordings.set(jobId, timer);
    }

    child.on('close', async code => {
      console.log(`Recording jobId=${jobId} ended with code=${code} after duration=${duration}`);

      try {
        await storeRecordingUrls(jobId);
        // Delete from S3
        await db.s3Client.send(new DeleteObjectCommand({
          Bucket: db.bucket,
          Key: `agent_recordings/${jobId}.json`
        }));
      } catch (err) {
        console.error(`Error deleting recording from S3:`, err);
      }

      activeTimers.recordings.delete(jobId);
    });

    res.status(201).json({ message: 'Recording job accepted', pid });
  }).catch(err => {
    console.error('Error storing recording job in S3:', err);
    try { process.kill(pid, 'SIGTERM'); } catch { }
    res.status(500).json({ error: err.message });
  });
});

// Stop a recording job
agentRouter.delete('/recordings/:jobId', async (req, res) => {
  const { jobId } = req.params;

  try {
    const getParams = {
      Bucket: db.bucket,
      Key: `agent_recordings/${jobId}.json`
    };

    try {
      const response = await db.s3Client.send(new GetObjectCommand(getParams));
      const dataStr = await streamToString(response.Body);
      const data = JSON.parse(dataStr);

      if (data.processPid) {
        try {
          process.kill(data.processPid, 'SIGTERM');
        } catch { }
      }

      await db.s3Client.send(new DeleteObjectCommand({
        Bucket: db.bucket,
        Key: `agent_recordings/${jobId}.json`
      }));

      // Clear any setTimeout
      if (activeTimers.recordings.has(jobId)) {
        clearTimeout(activeTimers.recordings.get(jobId));
        activeTimers.recordings.delete(jobId);
      }

      res.sendStatus(204);
    } catch (err) {
      if (err.name === 'NoSuchKey') {
        return res.status(404).json({ message: 'Not found' });
      }
      throw err;
    }
  } catch (err) {
    console.error('Error deleting recording job:', err);
    res.status(500).json({ error: err.message });
  }
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

  const now = new Date();
  let completedInserts = 0;
  const pids = [];
  let errors = 0;

  // Check if the jobId is already playing back
  try {
    const listParams = {
      Bucket: db.bucket,
      Prefix: `agent_playbacks/`,
      MaxKeys: 1000
    };

    const response = await db.s3Client.send(new ListObjectsV2Command(listParams));

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

          if (data.jobId === jobId && data.status === 'running') {
            // Confirm the process is still running
            try {
              console.warn('Playback job marked already running: jobId=', jobId, 'pid=', data.processPid, ' checking if still running');
              // Check if PID is still running
              process.kill(data.processPid, 0);

              // Kill the process
              console.warn('Killing existing playback:', data.processPid);
              try {
                process.kill(data.processPid, 'SIGTERM');
              } catch (killErr) {
                console.log('Error killing existing playback:', killErr);
              }

              // Delete from S3
              await db.s3Client.send(new DeleteObjectCommand({
                Bucket: db.bucket,
                Key: obj.Key
              }));

              console.warn('Deleted existing playback:', jobId);
            } catch {
              // Process not running, delete the record
              console.log('Playback job already running but process not found: jobId=', jobId);
              await db.s3Client.send(new DeleteObjectCommand({
                Bucket: db.bucket,
                Key: obj.Key
              }));
            }
          }
        } catch (err) {
          console.error(`Error reading ${obj.Key}:`, err);
        }
      }
    }
  } catch (err) {
    console.error('Error checking for existing playbacks:', err);
  }

  // Insert each child process
  for (const child of childArray) {
    const pid = child.pid;
    pids.push(pid);

    // Create a unique ID for this playback instance
    const playbackInstanceId = `${jobId}_${uuidv4()}`;

    const playbackData = {
      playbackInstanceId,
      jobId,
      sourceProfile,
      destinationUrl,
      duration,
      status: 'running',
      processPid: pid,
      startTime: now.toISOString()
    };

    try {
      await db.s3Client.send(new PutObjectCommand({
        Bucket: db.bucket,
        Key: `agent_playbacks/${playbackInstanceId}.json`,
        Body: JSON.stringify(playbackData)
      }));

      completedInserts++;

      // Set up auto-stop timer if duration > 0
      if (duration && duration > 0) {
        let max_duration = duration * 1.2;
        const timer = setTimeout(async () => {
          console.log(`Auto-stopping playback jobId=${jobId} after duration ${duration}`);
          try {
            process.kill(pid, 'SIGTERM');
          } catch { }

          try {
            await db.s3Client.send(new DeleteObjectCommand({
              Bucket: db.bucket,
              Key: `agent_playbacks/${playbackInstanceId}.json`
            }));
          } catch (err) {
            console.error('Error deleting playback from S3:', err);
          }

          activeTimers.playbacks.delete(playbackInstanceId);
        }, (max_duration) * 1000);

        activeTimers.playbacks.set(playbackInstanceId, timer);
      }

      // Set up process end handler
      child.on('close', async code => {
        console.log(`Playback jobId=${jobId} ended with code=${code}`);
        try {
          process.kill(pid, 'SIGTERM');
        } catch { }

        try {
          await db.s3Client.send(new DeleteObjectCommand({
            Bucket: db.bucket,
            Key: `agent_playbacks/${playbackInstanceId}.json`
          }));
        } catch (err) {
          console.error('Error deleting playback from S3:', err);
        }

        activeTimers.playbacks.delete(playbackInstanceId);
      });
    } catch (err) {
      console.error('Error storing playback in S3:', err);
      try { process.kill(pid, 'SIGTERM'); } catch { }
      errors++;
    }
  }

  if (completedInserts > 0) {
    console.log('Inserted all child processes:', completedInserts, ' of ', childArray.length, ' with ', errors, ' errors');
    return res.status(201).json({
      message: 'Playback job accepted',
      childCount: childArray.length,
      pids
    });
  } else {
    return res.status(400).json({ error: `Only ${completedInserts} child processes inserted of ${childArray.length} with ${errors} errors` });
  }
});

// Stop a playback job
agentRouter.delete('/playbacks/:jobId', async (req, res) => {
  const { jobId } = req.params;

  try {
    const listParams = {
      Bucket: db.bucket,
      Prefix: 'agent_playbacks/',
      MaxKeys: 1000
    };

    const response = await db.s3Client.send(new ListObjectsV2Command(listParams));
    let found = false;

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

          if (data.jobId === jobId) {
            found = true;

            if (data.processPid) {
              try {
                process.kill(data.processPid, 'SIGTERM');
              } catch { }
            }

            await db.s3Client.send(new DeleteObjectCommand({
              Bucket: db.bucket,
              Key: obj.Key
            }));

            // Clear any setTimeout
            if (activeTimers.playbacks.has(data.id)) {
              clearTimeout(activeTimers.playbacks.get(data.id));
              activeTimers.playbacks.delete(data.id);
            }
          }
        } catch (err) {
          console.error(`Error reading ${obj.Key}:`, err);
        }
      }
    }

    if (!found) {
      return res.status(404).json({ message: 'Not found' });
    }

    res.sendStatus(204);
  } catch (err) {
    console.error('Error deleting playback job:', err);
    res.status(500).json({ error: err.message });
  }
});

// Bind agentRouter under /v1/agent
app.use('/v1/agent', agentRouter);

// ----------------------------------------------------
// Start the server
// ----------------------------------------------------
app.listen(SERVER_PORT, () => {
  console.log(`Manager/Agent API Server version ${serverVersion} Manager@${serverUrl} AgentUrl@${agentUrl}.`);
  let capture_buffer_size = env.CAPTURE_BUFFER_SIZE || `4193904`;
  let segment_duration_ms = env.SEGMENT_DURATION_MS || `2000`;
  let minio_root_user = env.MINIO_ROOT_USER || `minioadmin`;
  let minio_root_password = env.MINIO_ROOT_PASSWORD || `minioadmin`;
  let url_signing_seconds = env.URL_SIGNING_SECONDS || `604800`;
  let use_estimated_duration = env.USE_ESTIMATED_DURATION || `true`;
  let max_segment_size_bytes = env.MAX_SEGMENT_SIZE_BYTES || `5242756`;

  const help_msg = `
Environment Variables:
  - SERVER_PORT: Port for the Node server to listen on as a Manager or Agent (default: ` + SERVER_PORT + `)
  - SERVER_HOST: Host for the Node server to listen on as a Manager or Agent (default: ` + SERVER_HOST + `)
  - AGENT_PORT: Port for the Agent server used by the Manager (default: ` + AGENT_PORT + `)
  - AGENT_HOST: Host for the Agent server used by the Manager (default: ` + AGENT_HOST + `)
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
  - HLS_DIR: Directory to change to before spawning hls-to-udp or udp-to-hls (default: ` + HLS_DIR + `)
  - ORIGINAL_DIR: Original directory before changing to HLS_DIR (default: ` + ORIGINAL_DIR + `)
  - SWAGGER_FILE: Path to the Swagger file (default: ` + SWAGGER_FILE + `)
`;
  console.log('\nRecord/Playback API Server Manager URL:', serverUrl, ' Agent URL:', agentUrl);
  console.log('Current Working Directory:', process.cwd());
  console.log('Storage Pool default S3 Endpoint:', s3endPoint);
  console.log('Swagger UI at:', serverUrl + '/api-docs');
  console.log(help_msg);
  /* check if env.HOURLY_URLS_LOG is defined, not empty and is a valid file, if not touch it and create it */
  if (env.HOURLY_URLS_LOG && env.HOURLY_URLS_LOG !== ""
    && fs.existsSync(env.HOURLY_URLS_LOG) && fs.lstatSync(env.HOURLY_URLS_LOG).isFile()) {
    console.log('Hourly URLs log file:', env.HOURLY_URLS_LOG);
  }
  else if (env.HOURLY_URLS_LOG && env.HOURLY_URLS_LOG !== "") {
    console.log('Hourly URLs log file defined and not found, creating it:', env.HOURLY_URLS_LOG);
    fs.writeFileSync(env.HOURLY_URLS_LOG, '');
  }
  console.log(`\nRecord/Playback Server started at: ${new Date().toISOString()}\n- Listening for connections...\n`);
});
