# MPegTS Pid Tracker

Track MpegTS PIDs and Continuity Counter errors plus general correctness of packets and alignment value. It is used by the udp-to-hls crate and the hls-to-udp crates at https://github.com/groovybits/mpegts-to-s3.

## Overview

The MPegTS Pid Tracker is a utility for processing MPEG-TS packets by tracking their Packet IDs (PIDs) and ensuring the continuity of the associated counters. It helps detect issues in packet streams such as missing or out-of-order packets and gracefully handles discontinuities signaled in the packet adaptation field.

### Key Features

- Processes TS packets with a fixed size of 188 bytes.
- Extracts the PID from each packet and skips null packets.
- Checks for the presence of a payload before performing continuity validation.
- Automatically resets the continuity counter when a discontinuity is detected.
- Logs errors when a continuity mismatch is found, ensuring that tracking continues without compounding errors.

### Example Usage

The included code example demonstrates how to use the tracker:

1. Initialize a new PID tracker.
2. Open a file stream (or switch to a fallback basic packet blob if the file doesn't exist).
3. Read TS packets sequentially.
4. Update the continuity counter for each non-null packet, verifying that the sequence is correct.
5. Log any continuity issues associated with a particular PID.

This practical approach makes it easy to integrate the tracker into streaming applications or diagnostic tools requiring analysis of MPEG-TS streams.

## API Reference and Usage

The `PidTracker` struct provides the following methods:
### new()
Creates a new instance of the PID tracker with an empty continuity map.

**Signature:**
```rust
pub fn new() -> Self
```

**Example:**
```rust
let tracker = PidTracker::new();
```

---

### get_counter(pid: u16)
Retrieves the last stored continuity counter for the specified PID. Returns `Some(counter)` if the PID has been processed previously, or `None` if it is not present.

**Signature:**
```rust
pub fn get_counter(&self, pid: u16) -> Option<u8>
```

**Example:**
```rust
if let Some(cc) = tracker.get_counter(100) {
    println!("Last continuity counter for PID 100: {}", cc);
}
```

---

### process_packet(label: String, packet: &[u8])
Processes a single MPEG-TS packet by:
- Validating the packet size.
- Parsing the packet to extract the PID and continuity counter.
- Skipping null packets (PID = 0x1FFF).
- Determining if the packet contains a payload.
- Checking the adaptation field for a discontinuity indicator.
- Validating the continuity counter when there is a payload and no discontinuity.
- Logging and returning an error if a continuity mismatch is detected.

**Signature:**
```rust
pub fn process_packet(&mut self, label: String, packet: &[u8]) -> Result<(), u16>
```

**Example:**
```rust
if let Err(bad_pid) = tracker.process_packet(String::from("stream.ts"), &packet) {
    println!("Continuity error for PID: {}", bad_pid);
}
```

**Return Values:**
- Returns `Ok(())` if the packet is processed successfully, including cases where no payload is processed.
- Returns `Err(pid)` if a continuity mismatch is detected, where `pid` is the identifier with the error.
- Returns `Err(0xFFFF)` for general errors like invalid packet size or incorrect adaptation field length.


## License

This project is licensed under the MIT License.

## Acknowledgements

This project was inspired by the need to track MPEG-TS packet continuity counters in a streaming application.