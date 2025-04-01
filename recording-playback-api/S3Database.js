/****************************************************
 * S3Database.js — Recording/Playback API S3 Database Class
 * 
 * - Author: CK <ck@groovybits> https://github.com/groovybits/mpegts_to_s3
 ****************************************************/
import config from './config.js';

import {
  S3Client,
  PutObjectCommand,
  GetObjectCommand,
  DeleteObjectCommand,
  paginateListObjectsV2
} from '@aws-sdk/client-s3';

const s3endPoint = config.s3endPoint;
const s3Region = config.s3Region;
const s3AccessKeyDB = config.s3AccessKeyDB;
const s3SecretKeyDB = config.s3SecretKeyDB;
const s3BucketDB = config.s3BucketDB;
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

  // Helper to convert a stream to a string
  async streamToString(stream) {
    const chunks = [];
    return new Promise((resolve, reject) => {
      stream.on('data', (chunk) => chunks.push(Buffer.from(chunk)));
      stream.on('error', (err) => reject(err));
      stream.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    });
  }

  async getS3DataWithKey(prefix) {
    const listParams = {
      Bucket: this.bucket,
      Prefix: prefix,
      MaxKeys: 1000
    };

    const paginator = paginateListObjectsV2({ client: this.s3Client }, listParams);
    const rows = [];

    for await (const page of paginator) {
      if (page.Contents) {
        for (const obj of page.Contents) {
          const getParams = {
            Bucket: this.bucket,
            Key: obj.Key
          };
          try {
            const itemResponse = await this.s3Client.send(new GetObjectCommand(getParams));
            const dataStr = await this.streamToString(itemResponse.Body);
            const data = JSON.parse(dataStr);
            rows.push({ key: obj.Key, data });
          } catch (err) {
            console.error(`Error reading ${obj.Key}:`, err);
          }
        }
      }
    }
    return rows;
  }

  /**
   * Looks up pool credentials by profile name/ID
   * @param {string} profileId - The pool ID or profile name to look up
   * @returns {Promise<{bucketName: string, accessKey: string, secretKey: string}|null>}
   */
  async getPoolCredentials(profileId) {
    if (!profileId || profileId === 'default') {
      // Return default credentials
      return {
        bucketName: this.bucket,
        accessKey: this.s3Client.config.credentials.accessKeyId || s3AccessKeyDB,
        secretKey: this.s3Client.config.credentials.secretAccessKey || s3SecretKeyDB
      };
    }

    try {
      // Lookup the pool in S3
      const getParams = {
        Bucket: this.bucket,
        Key: `pools/${profileId}.json`
      };

      console.log('getPoolCredentials: Looking up pool:', profileId);

      try {
        const response = await this.s3Client.send(new GetObjectCommand(getParams));
        const dataStr = await this.streamToString(response.Body);
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

  // Function to retrieve and process S3 objects given bucket and prefix.
  async getS3Data(prefix) {
    const listParams = {
      Bucket: this.bucket,
      Prefix: prefix,
      MaxKeys: 1000
    };

    const paginator = paginateListObjectsV2({ client: this.s3Client }, listParams);
    const rows = [];

    for await (const page of paginator) {
      if (page.Contents) {
        for (const obj of page.Contents) {
          const getParams = {
            Bucket: this.bucket,
            Key: obj.Key
          };
          try {
            const itemResponse = await this.s3Client.send(new GetObjectCommand(getParams));
            const dataStr = await this.streamToString(itemResponse.Body);
            const data = JSON.parse(dataStr);
            rows.push(data);
          } catch (err) {
            console.error(`Error reading ${obj.Key}:`, err);
          }
        }
      }
    }
    return rows;
  }

  // Function to get data from S3 using a specific key
  async get(key) {
    try {
      const getParams = {
        Bucket: db.bucket,
        Key: key
      };

      try {
        const response = await db.s3Client.send(new GetObjectCommand(getParams));
        const dataStr = await db.streamToString(response.Body);
        const data = JSON.parse(dataStr);
        return data;
      } catch (err) {
        if (err.name === 'NoSuchKey') {
          console.error(`Error with recording ${recordingId} not found with error:`, err.name, err.message);
          return null;
        }
        throw err;
      }
    } catch (err) {
      console.error(`Error getting recording ${recordingId}:`, err);
      return null;
    }
  }

  // Function to put data into S3
  async put(key, data) {
    try {
      const putParams = {
        Bucket: this.bucket,
        Key: key,
        Body: JSON.stringify(data)
      };

      await this.s3Client.send(new PutObjectCommand(putParams));
    } catch (err) {
      console.error(`Error putting data into S3 for key ${key}:`, err);
      throw err;
    }
  }

  // Function to delete data from S3
  async delete(key) {
    try {
      const deleteParams = {
        Bucket: this.bucket,
        Key: key
      };

      await this.s3Client.send(new DeleteObjectCommand(deleteParams));
    } catch (err) {
      console.error(`Error deleting data from S3 for key ${key}:`, err);
      throw err;
    }
  }
}