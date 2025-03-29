## Record / Playback Manager and Agent API TODO

### CRITICAL BUGS
- [ ] Smoother can have some bursts every few minutes potentially (WIP)
- [ ] Avoid losing time in the capture. (may be smoother issue)

### NON-CRITICAL BUGS
- [ ] Timing output could be given higher accuracy
- [X] Fix capture duration to be exact

### FEATURES
- [ ] Improve duration derivation in segmentation, precision
- [ ] Add policy support with more details to S3 signing
- [ ] Add Discontinuity flagging in HLS Manifest
- [ ] Add more testing and wiki documentation
- [X] Live mode vs. VOD mode for input/output buffering expectations
- [X] Give start/stop in/out edit points against walltime for retreival
- [X] inner playlists for HLS style multi-stream urls
- [ ] fix to allow  ?.. arg flags to m3u8 urls

### Swagger API Implementation recording-playback-api TASKS
- [ ] Add start/end time functionality support for playback in HLS
- [ ] Add Authentication Bearers handling to API for outh2 etc.
- [X] Implement attaching Pools of S3 bucket/creds to each API call properly
- [X] Implement filling a Pool with Assets as they are created.
- [X] Implement getting a list of all assets in a pool
- [ ] Add to and advance the stats and admin API endpoints.
- [X] Work out and test Manager/Agent configurations separated across servers.
- [ ] Implement multiple Agents per Manager and general scaling, queueing, etc.
- [ ] Add Unit Tests for each API endpoint and all the above features.
- [X] Merge m3u8's into a single m3u8 for HLS and serve the m3u8 as a single file.
- [ ] Pagination for S3 listing on assets and pools
- [X] Split out common s3 code into a shared library for both manager and agent.
- [ ] AWS Region and Endpoint settings for S3 stored per pool to allow multiple s3 region buckets and endpoints.
- [ ] Renew expired signing of URLs for playback and recording, create URL signing service for expiration handling.

---
March 2025 - CK 
