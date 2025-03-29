import {
  S3Client,
  PutObjectCommand,
  GetObjectCommand,
  ListObjectsV2Command,
} from '@aws-sdk/client-s3';
import fs from 'fs';
import { URL } from 'url';
import { env } from 'process';

// S3 endpoint for the MinIO server
const s3endPoint = process.env.AWS_S3_ENDPOINT || 'http://127.0.0.1:9000';
const s3Region = process.env.AWS_REGION || 'us-east-1';
const s3AccessKeyDB = process.env.AWS_ACCESS_KEY_ID || 'minioadmin';
const s3SecretKeyDB = process.env.AWS_SECRET_ACCESS_KEY || 'minioadmin';
const s3BucketDB = process.env.AWS_S3_BUCKET || 'media';

const ORIGINAL_DIR = process.cwd() + '/';
const HLS_DIR = process.env.HLS_DIR || '';

/**
 * S3Database - A class to handle database operations using S3 and JSON files
 * Each table is stored as a collection of JSON files in an S3 bucket
 */
export default class S3Database {
  constructor(endpoint = s3endPoint, bucket = s3BucketDB, region = s3Region, accessKey = null, secretKey = null) {
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