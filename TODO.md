## Record / Playback Manager and Agent API TODO

### CRITICAL BUGS
- [ ] Smoother can have some bursts every few minutes potentially (WIP)

### NON-CRITICAL BUGS
- [ ] Timing output could be given higher accuracy

### FEATURES
- [ ] Improve duration derivation in segmentation, precision
- [ ] Add policy support with more details to S3 signing
- [ ] Add Discontinuity flagging in HLS Manifest
- [ ] Add more testing and wiki documentation
- [X] Live mode vs. VOD mode for input/output buffering expectations
- [X] Give start/stop in/out edit points against walltime for retreival
- [X] inner playlists for HLS style multi-stream urls
- [ ] fix to allow  ?.. arg flags to m3u8 urls

### Swagger API Implementation recording-playback-server TASKS
- [ ] Add start/end time functionality support for playback in HLS
- [ ] Add Authentication Bearers handling to API for outh2 etc.
- [X] Implement attaching Pools of S3 bucket/creds to each API call properly
- [ ] Implement filling a Pool with Assets as they are created.
- [ ] Implement getting a list of all assets in a pool
- [ ] Add to and advance the stats and admin API endpoints.
- [ ] Work out and test Manager/Agent configurations separated across servers.
- [ ] Implement multiple Agents per Manager and general scaling, queueing, etc.
- [ ] Add Unit Tests for each API endpoint and all the above features.

---
January 2025 - @bitbytebit 
