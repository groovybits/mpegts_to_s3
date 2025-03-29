/****************************************************
 * agent.js — Recording/Playback API Agent
 * 
 * Environment Variables:
 * - AGENT_ID: Unique identifier for this agent (default: agent-001)
 * - SERVER_PORT: Port for the server to listen on (default: 3001)
 * - SERVER_HOST: Host for the server to listen on (default: 127.0.0.1)
 * - AWS_S3_ENDPOINT: Endpoint for the S3 server (default: http://127.0.0.1:9000)
 * - AWS_REGION: AWS region for S3 (default: us-east-1)
 * - AWS_ACCESS_KEY_ID: Access key for S3 (default: minioadmin)
 * - SMOOTHER_LATENCY: Smoother latency for hls-to-udp (default: 500)
 * - VERBOSE: Verbosity level for hls-to-udp (default: 2)
 * - UDP_BUFFER_BYTES: Buffer size for hls-to-udp (default: 0)
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
 * 
 * - Author: CK <ck@groovybits> https://github.com/groovybits/mpegts_to_s3
 * 
 ****************************************************/

const serverVersion = '1.1.3';

import express from 'express';
import { v4 as uuidv4 } from 'uuid';
import { spawn } from 'child_process';
import {
  PutObjectCommand,
  GetObjectCommand,
  ListObjectsV2Command,
  DeleteObjectCommand
} from '@aws-sdk/client-s3';
import fs from 'fs';
import http from 'http';
import https from 'https';
import { URL } from 'url';
import swaggerUi from 'swagger-ui-express';
import yaml from 'js-yaml';
import { env } from 'process';

import S3Database from './S3Database.js'; // Import the S3Database class

// Agent ID
const AGENT_ID = process.env.AGENT_ID || 'agent-001';
// Server Manager and Agent URL Bases used for fetch calls (same server in this case)
const SERVER_PROTOCOL = process.env.SERVER_PROTOCOL || 'http';
const SERVER_PORT = process.env.SERVER_PORT || 3001;
const SERVER_HOST = process.env.SERVER_HOST || "127.0.0.1"; // Manager and local Agents base server
const serverUrl = SERVER_PROTOCOL + '://' + SERVER_HOST + ':' + SERVER_PORT;

// S3 endpoint for the MinIO server
const s3endPoint = process.env.AWS_S3_ENDPOINT || 'http://127.0.0.1:9000';
const s3Region = process.env.AWS_REGION || 'us-east-1';
const s3AccessKeyDB = process.env.AWS_ACCESS_KEY_ID || 'minioadmin';
const s3SecretKeyDB = process.env.AWS_SECRET_ACCESS_KEY || 'minioadmin';
const s3BucketDB = process.env.AWS_S3_BUCKET || 'media';

// Runtime verbosity levels of Rust programs, 0-4: error, warn, info, debug, trace
const PLAYBACK_VERBOSE = process.env.PLAYBACK_VERBOSE || 2;
const RECORDING_VERBOSE = process.env.RECORDING_VERBOSE || 2;

// check env and set the values for the baseargs, else set to defaults, use vars below
const SMOOTHER_LATENCY = process.env.SMOOTHER_LATENCY || 100;
const UDP_BUFFER_BYTES = process.env.UDP_BUFFER_BYTES || 0;

// setup directorie paths and locations of files
const SWAGGER_FILE = process.env.SWAGGER_FILE || 'swagger_agent.yaml';
const ORIGINAL_DIR = process.cwd() + '/';
const HLS_DIR = process.env.HLS_DIR || '';

const RECIEVER_POLL_MS = process.env.RECIEVER_POLL_MS || 10; // Polling interval for hls-to-udp

// Queue sizes for hls-to-udp
const SEGMENT_QUEUE_SIZE = process.env.SEGMENT_QUEUE_SIZE || 1;
const UDP_QUEUE_SIZE = process.env.UDP_QUEUE_SIZE || 1;

// Add ../bin/ to PATH env variable if it exists
if (fs.existsSync('../bin/')) {
  process.env.PATH = process.env.PATH + ':../bin';
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
  const hourly_urls = env.HOURLY_URLS_LOG ? env.HOURLY_URLS_LOG : ORIGINAL_DIR + HLS_DIR + 'index.txt';

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
const db = new S3Database(s3endPoint, s3BucketDB, s3Region, s3AccessKeyDB, s3SecretKeyDB);

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
// AGENT ENDPOINTS (/v1/agent)
// ===============================
const agentRouter = express.Router();

/**
 * Helper to spawn udp-to-hls with appropriate args
 * Returns: childProcess
 */
async function spawnUdpToHls(jobId, sourceUrl, duration, destinationProfile) {
  // Attempt to parse the sourceUrl as a UDP URL
  const parsed = parseUdpUrl(sourceUrl);
  if (!parsed) {
    // Fallback: you might handle SRT or other protocols
    // For now, we'll just throw
    throw new Error(`Invalid or non-UDP sourceUrl: ${sourceUrl}`);
  }

  // Get pool credentials for the specified profile
  const poolCredentials = await getPoolCredentials(destinationProfile);
  if (!poolCredentials) {
    throw new Error(`Invalid destination profile: ${destinationProfile}`);
  }

  const { ip, port, iface } = parsed;
  const { bucketName, accessKey, secretKey } = poolCredentials;

  const env = {
    ...process.env,
    MINIO_ROOT_USER: accessKey,
    MINIO_ROOT_PASSWORD: secretKey
  }

  // Build command arguments with credentials
  const args = [
    '-n', iface,
    '-v', `${RECORDING_VERBOSE}`,
    '-e', s3endPoint,
    '-b', bucketName,
    //'--duration', duration + 1, // Add 1 second to duration
    '--hls_keep_segments', '0',     // Keep all segments
    '-i', ip,
    '-p', port,
    '-o', jobId  // acts as the "channel name"/local output folder
  ];

  console.log(`Spawning => udp-to-hls ${args.join(' ')}`);

  const child = spawn('udp-to-hls', args, {
    stdio: ['ignore', 'pipe', 'pipe'],
    env: env
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
 * Playlist combiner and HTTP server for serving a single merged playlist
 * that references the original segment URLs directly.
 */

/**
 * Fetches and parses an m3u8 playlist
 */
async function fetchPlaylist(playlistUrl) {
  return new Promise((resolve, reject) => {
    const parsedUrl = new URL(playlistUrl);
    const protocol = parsedUrl.protocol === 'https:' ? https : http;

    const options = {
      hostname: parsedUrl.hostname,
      port: parsedUrl.port || (parsedUrl.protocol === 'https:' ? 443 : 80),
      path: parsedUrl.pathname + parsedUrl.search,
      method: 'GET'
    };

    const req = protocol.request(options, (res) => {
      if (res.statusCode !== 200) {
        reject(new Error(`Failed to fetch playlist: ${res.statusCode}`));
        return;
      }

      let data = '';
      res.on('data', (chunk) => data += chunk);
      res.on('end', () => {
        try {
          const lines = data.trim().split('\n');
          const segments = [];

          let currentSegment = null;
          let absoluteTime = null;

          for (const line of lines) {
            if (line.startsWith('#EXT-X-PROGRAM-DATE-TIME:')) {
              absoluteTime = new Date(line.substring(25).trim());
            }
            else if (line.startsWith('#EXTINF:')) {
              // format #EXTINF:0.285717,
              const durationMatch = line.match(/#EXTINF:([\d.]+),/);
              if (durationMatch) {
                const duration = parseFloat(durationMatch[1]);
                if (isNaN(duration)) {
                  console.warn(`Invalid duration found: ${line}`);
                } else {
                  currentSegment = {
                    duration,
                    extinf: line,
                    absoluteTime: absoluteTime ? new Date(absoluteTime) : null
                  };
                  if (absoluteTime) {
                    absoluteTime = new Date(absoluteTime.getTime() +
                      (duration * 1000));
                  }
                }
              }
            }
            else if (line.trim() !== '' && !line.startsWith('#') && currentSegment) {
              // Resolve relative URLs to absolute
              let segmentUrl = line.trim();
              if (!segmentUrl.startsWith('http')) {
                const baseUrl = new URL(playlistUrl);
                if (segmentUrl.startsWith('/')) {
                  segmentUrl = `${baseUrl.protocol}//${baseUrl.host}${segmentUrl}`;
                } else {
                  const urlDir = playlistUrl.substring(0, playlistUrl.lastIndexOf('/') + 1);
                  segmentUrl = `${urlDir}${segmentUrl}`;
                }
              }

              currentSegment.uri = segmentUrl;
              segments.push(currentSegment);
              currentSegment = null;
            }
          }

          resolve(segments);
        } catch (error) {
          reject(error);
        }
      });
    });

    req.on('error', (error) => reject(error));
    req.end();
  });
}

/**
 * Combines multiple playlists and optionally trims by time range
 */
async function combineAndTrimPlaylists(playlistUrls, startTime, endTime) {
  try {
    // Fetch all playlists
    const segmentArrays = await Promise.all(
      playlistUrls.map(url => fetchPlaylist(url))
    );

    // Combine all segments
    let allSegments = [];
    for (const segments of segmentArrays) {
      allSegments = allSegments.concat(segments);
    }

    // Sort segments by absolute time if available
    allSegments.sort((a, b) => {
      if (a.absoluteTime && b.absoluteTime) {
        return a.absoluteTime.getTime() - b.absoluteTime.getTime();
      }
      return 0;
    });

    // Filter segments by time range if specified
    if (startTime && endTime) {
      const startTimeDate = new Date(startTime);
      const endTimeDate = new Date(endTime);

      allSegments = allSegments.filter(segment => {
        if (!segment.absoluteTime) return true;
        return segment.absoluteTime >= startTimeDate && segment.absoluteTime <= endTimeDate;
      });
    }

    // Max duration calculation
    let maxDuration = 1; // default safe fallback
    for (let i = 0, len = allSegments.length; i < len; i++) {
      const duration = Number(allSegments[i].duration);
      if (!isNaN(duration) && duration > maxDuration) {
        maxDuration = duration;
      }
    }
    maxDuration = Math.ceil(maxDuration);

    console.log('Found Playlist Max duration:', maxDuration);

    // Build the combined playlist
    let combinedContent = '#EXTM3U\n';
    combinedContent += '#EXT-X-VERSION:3\n';
    combinedContent += `#EXT-X-TARGETDURATION:${maxDuration}\n`;
    combinedContent += '#EXT-X-MEDIA-SEQUENCE:0\n';

    for (const segment of allSegments) {
      if (segment.absoluteTime) {
        combinedContent += `#EXT-X-PROGRAM-DATE-TIME:${segment.absoluteTime.toISOString()}\n`;
      }
      combinedContent += `${segment.extinf}\n`;
      combinedContent += `${segment.uri}\n`;
    }

    combinedContent += '#EXT-X-ENDLIST\n';

    return combinedContent;
  } catch (error) {
    console.error('Error combining playlists:', error);
    throw error;
  }
}

/**
 * Creates a simple HTTP server to serve the combined playlist
 */
function createPlaylistServer(playlistContent, port = 0) {
  return new Promise((resolve, reject) => {
    try {
      const server = http.createServer((req, res) => {
        if (req.url === '/playlist.m3u8') {
          res.writeHead(200, {
            'Content-Type': 'application/vnd.apple.mpegurl',
            'Access-Control-Allow-Origin': '*',
            'Cache-Control': 'no-cache'
          });
          res.end(playlistContent);
        } else {
          res.writeHead(404);
          res.end('Not Found');
        }
      });

      server.listen(port, () => {
        const addressInfo = server.address();
        const serverUrl = `http://localhost:${addressInfo.port}/playlist.m3u8`;
        resolve({ server, url: serverUrl });
      });

      server.on('error', (error) => reject(error));
    } catch (error) {
      reject(error);
    }
  });
}

/**
 * Helper to spawn hls-to-udp with a combined playlist
 */
async function spawnHlsToUdpWithCombinedPlaylist(jobId, destinationUrl, vodStartTime, vodEndTime) {
  if (!jobId) {
    console.log('Invalid jobId:', jobId);
    return null;
  }

  // Parse the destinationUrl to get IP and port
  const parsedDest = parseUdpUrl(destinationUrl);
  if (!parsedDest) {
    throw new Error(`Invalid or non-UDP destinationUrl: ${destinationUrl}`);
  }
  const { ip, port } = parsedDest;

  // Get playlist URLs
  let vodPlaylists = [];
  try {
    vodPlaylists = await getVodPlaylists(jobId, vodStartTime, vodEndTime);
  } catch (err) {
    console.error('Error getting vod playlists from S3:', err);
    return null;
  }

  if (vodPlaylists.length === 0) {
    console.log('No playlists found for the specified criteria');
    return null;
  }

  console.log(`Found ${vodPlaylists.length} playlists for job ${jobId}`);

  // Combine and trim playlists
  console.log('Combining and trimming playlists...');
  const combinedPlaylist = await combineAndTrimPlaylists(
    vodPlaylists,
    vodStartTime,
    vodEndTime
  );

  // Create HTTP server to serve the combined playlist
  console.log('Starting playlist server for ', vodPlaylists.length, ' playlists spanning ', vodStartTime, ' to ', vodEndTime);
  const { server, url: playlistUrl } = await createPlaylistServer(combinedPlaylist);

  console.log(`Serving combined playlist at: ${playlistUrl}`);

  // Build arguments for hls-to-udp
  const baseArgs = [
    '--vod',
    '--use-smoother',
    '-l', `${SMOOTHER_LATENCY}`,
    '-b', `${UDP_BUFFER_BYTES}`,
    '-p', `${RECIEVER_POLL_MS}`,
    '-v', `${PLAYBACK_VERBOSE}`,
    '-u', playlistUrl,
    '-o', `${ip}:${port}`,
    '-q', `${SEGMENT_QUEUE_SIZE}`,
    '-z', `${UDP_QUEUE_SIZE}`
  ];

  console.log(`Spawning hls-to-udp with combined playlist => ${baseArgs.join(' ')}`);
  const child = spawn('hls-to-udp', baseArgs, {
    stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env }
  });

  // Attach listeners for stdout and stderr
  child.stdout.on('data', data => {
    console.log(`hls-to-udp stdout: ${data.toString().trim()}`);
  });
  child.stderr.on('data', data => {
    console.error(`hls-to-udp stderr: ${data.toString().trim()}`);
  });

  // Clean up the server when the process exits
  child.on('close', code => {
    console.log(`hls-to-udp process ended with code=${code}, stopping playlist server`);
    server.close();
  });

  child.on('error', err => {
    console.error('Error in hls-to-udp process:', err);
    server.close();
  });

  // Store the server reference on the child so we can close it if needed
  child.server = server;

  return child;
}

/**
 * Helper to spawn hls-to-udp in VOD mode with a combined playlist
 * Returns: Promise resolving to an array containing a single spawned child process.
 */
async function spawnHlsToUdp(jobId, destinationUrl, vodStartTime, vodEndTime) {
  try {
    const child = await spawnHlsToUdpWithCombinedPlaylist(jobId, destinationUrl, vodStartTime, vodEndTime);
    if (!child) {
      console.error('Failed to spawn hls-to-udp process');
      return [];
    }
    return [child]; // Return as array for backward compatibility
  } catch (err) {
    console.error('Error in spawnHlsToUdp:', err);
    return [];
  }
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
      agentId: AGENT_ID,
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
agentRouter.post('/jobs/recordings', async (req, res) => {
  const { jobId, sourceUrl, duration, destinationProfile } = req.body;
  if (!jobId || !sourceUrl) {
    return res.status(400).json({ error: 'Missing jobId or sourceUrl' });
  }

  let child;
  try {
    child = await spawnUdpToHls(jobId, sourceUrl, duration, destinationProfile);
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
  const { jobId, recordingId, destinationUrl, duration, vodStartTime, vodEndTime } = req.body;
  if (!jobId || !destinationUrl) {
    return res.status(400).json({ error: 'Missing jobId or destinationUrl' });
  }

  let childArray;
  try {
    childArray = await spawnHlsToUdp(recordingId, destinationUrl, vodStartTime, vodEndTime);
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
  console.log(`Recording / Playback Agent API Server AgentID: [${AGENT_ID}] Version: ${serverVersion} AgentURL: ${serverUrl}`);
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
  - AWS_S3_ENDPOINT: Default Endpoint for the S3 storage pool server (default: ` + s3endPoint + `)
  - AWS_S3_REGION: Default Region for the S3 storage pool server (default: ` + s3Region + `)
  - SMOOTHER_LATENCY: Smoother latency for hls-to-udp output (default: ` + SMOOTHER_LATENCY + `)
  - PLAYBACK_VERBOSE: Verbosity level for hls-to-udp (default: ` + PLAYBACK_VERBOSE + `)
  - RECORDING_VERBOSE: Verbosity level for udp-to-hls (default: ` + RECORDING_VERBOSE + `)
  - UDP_BUFFER_BYTES: Buffer size for hls-to-udp (default: ` + UDP_BUFFER_BYTES + `)
  - CAPTURE_BUFFER_SIZE: Buffer size for udp-to-hls (default: ` + capture_buffer_size + `)
  - MINIO_ROOT_USER: Default S3 Access Key (default: ` + minio_root_user + `)
  - MINIO_ROOT_PASSWORD: Default S3 Secret Key (default: ` + minio_root_password + `)
  - URL_SIGNING_SECONDS: S3 URL Signing time duration (default: ` + url_signing_seconds + `)
  - SEGMENT_DURATION_MS: ~Duration (estimate) of each segment in milliseconds (default: ` + segment_duration_ms + `)
  - MAX_SEGMENT_SIZE_BYTES: Maximum size of each segment in bytes (default: ` + max_segment_size_bytes + `)
  - USE_ESTIMATED_DURATION: Use estimated duration for recording (default: ` + use_estimated_duration + `)
  - HLS_DIR: Directory to change to before spawning hls-to-udp or udp-to-hls (default: ` + HLS_DIR + `)
  - ORIGINAL_DIR: Original directory before changing to HLS_DIR (default: ` + ORIGINAL_DIR + `)
  - SWAGGER_FILE: Path to the Swagger file (default: ` + SWAGGER_FILE + `)
`;
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
  console.log(`\nRecord/Playback Agent started at: ${new Date().toISOString()}\n- Listening for connections...\n`);
});
