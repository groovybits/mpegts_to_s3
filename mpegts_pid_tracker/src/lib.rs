use std::collections::HashMap;

pub const TS_PACKET_SIZE: usize = 188;
pub const PID_NULL: u16 = 0x1FFF;

/// Represents a tracker for MPEG-TS packet identifier (PID) continuity counters.
///
/// The `PidTracker` maintains a mapping between PIDs (as `u16`)
/// and their corresponding continuity counter values (as `u8`).
/// This is useful for detecting discontinuities or losses in packet streams,
/// especially in scenarios involving MPEG-TS packet manipulation and analysis.
///
/// # Examples
///
/// Basic usage with an initial PID-to-continuity mapping:
///
/// ```rust
/// use std::collections::HashMap;
///
/// // Create a new mapping for PID continuity counters.
/// let mut continuity_map = HashMap::new();
/// continuity_map.insert(256, 10);
///
/// // Initialize the PID tracker with the given mapping.
/// let pid_tracker = PidTracker {
///     continuity: continuity_map,
/// };
///
/// // Validate that the continuity counter for PID 256 is correctly set.
/// assert_eq!(pid_tracker.continuity.get(&256), Some(&10));
/// ```
///
/// Additional manipulation of the tracker can involve updating or adding new PID values.
/// This structure serves as a foundation for more advanced packet tracking and processing logic.
pub struct PidTracker {
    continuity: HashMap<u16, u8>,
}

/// Implementation of the `PidTracker` struct.
/// This structure maintains a mapping between MPEG-TS packet identifiers (PIDs)
/// and their corresponding continuity counter values.
/// The continuity counter is a 4-bit field that increments with each packet,
/// allowing for the detection of missing or out-of-order packets in a stream.
/// The `PidTracker` structure provides methods for updating and querying the continuity counters
/// for specific PIDs, as well as processing individual transport stream (TS) packets to update the counters.
/// The `process_packet` method is used to update the continuity counter for a given PID based on the contents of a TS packet.
///     - The method checks for the presence of a payload in the packet and respects the continuity “discontinuity_indicator” in the adaptation field if present.
///    - It only increments or checks the continuity counter if the packet has a payload.
///   - If a discontinuity is detected, the continuity counter is reset for that PID.
/// - The method logs an error and returns an error value containing the PID in case of a continuity mismatch.
///
/// # Example
///
/// ```rust
/// use mpegts_pid_tracker::PidTracker;
///
/// // Create a new PID tracker.
/// let mut tracker = PidTracker::new();
///
/// // Process a TS packet to update the continuity counter for a PID.
/// let label = String::from("test.ts");
/// let packet = [0u8; 188];
/// if let Err(pid) = tracker.process_packet(label, &packet) {
///    println!("Error with PID: {}", pid);
/// }
/// ```
///
impl PidTracker {
    /// Create a new PID tracker.
    pub fn new() -> Self {
        Self {
            continuity: HashMap::new(),
        }
    }

    /// Retrieve the last stored continuity counter for the given PID, if any.
    pub fn get_counter(&self, pid: u16) -> Option<u8> {
        self.continuity.get(&pid).copied()
    }

    /// Process a TS packet, updating the stored continuity counter for its PID.
    ///
    /// Functionality:
    /// - Skips null packets (PID = 0x1FFF)
    /// - Checks whether the packet contains a payload (based on adaptation_field_control).
    /// - Respects the continuity “discontinuity_indicator” in the adaptation field if present.
    /// - Only increments/checks continuity if the packet has a payload.
    /// - Logs an error and returns `Err(pid)` on continuity mismatches.
    pub fn process_packet(&mut self, label: String, packet: &[u8]) -> Result<(), u16> {
        if packet.len() != TS_PACKET_SIZE {
            log::error!(
                "PidTracker: ({}) Packet size is incorrect (got {}, expected {}).",
                label,
                packet.len(),
                TS_PACKET_SIZE
            );
            // Use 0xFFFF as a generic error signal for invalid packet size.
            return Err(0xFFFF);
        }

        // --- Parse PID ---
        //  Packet[1], high 5 bits => bits of PID
        //  Packet[2], low 8 bits => bits of PID
        let pid = (((packet[1] & 0x1F) as u16) << 8) | (packet[2] as u16);

        // Skip continuity checks for the null packet (PID 0x1FFF).
        if pid == PID_NULL {
            return Ok(());
        }

        // --- Parse continuity counter ---
        let current_cc = packet[3] & 0x0F;

        // Adaptation field control is in bits [4..6) of packet[3].
        //  0b00 = Reserved
        //  0b01 = Payload only
        //  0b10 = Adaptation only
        //  0b11 = Adaptation + payload
        let adaptation_field_control = (packet[3] >> 4) & 0x03;
        let has_adaptation = (adaptation_field_control & 0b10) != 0;
        let has_payload = (adaptation_field_control & 0b01) != 0;

        // --- Check for discontinuity_indicator in adaptation field ---
        let mut discontinuity_indicator = false;
        if has_adaptation {
            // Byte 4 = adaptation_field_length
            let adaptation_length = packet[4] as usize;
            // Minimal sanity check; max allowed is 183 because:
            //   188 total bytes - 4 bytes of header - 1 byte of 'length' = 183
            if adaptation_length > 183 {
                log::error!(
                    "PidTracker: ({}) Adaptation field length {} is invalid for PID {}.",
                    label,
                    adaptation_length,
                    pid
                );
                // Return an error or skip processing.
                return Err(0xFFFF);
            }

            // If adaptation_length > 0, the next byte has the flags.
            if adaptation_length > 0 {
                // Byte 5 = adaptation_field_flags
                let adaptation_flags = packet[5];
                // The top bit (0x80) is the 'discontinuity_indicator'.
                // (Bit positions: 0x80=discontinuity_indicator, 0x40=random_access_indicator, etc.)
                discontinuity_indicator = (adaptation_flags & 0x80) != 0;
            }
        }

        // If the packet has NO payload, the continuity counter is not supposed to increment,
        // so skip the continuity check entirely.
        if !has_payload {
            // We neither check nor update the continuity counter in this case.
            return Ok(());
        }

        // If the stream signaled a discontinuity, we typically "forgive" or reset
        // the continuity sequence for that PID and accept whatever CC is present now.
        if discontinuity_indicator {
            self.continuity.insert(pid, current_cc);
            return Ok(());
        }

        // --- Normal continuity check (only if there's a payload and no discontinuity) ---
        if let Some(&last_cc) = self.continuity.get(&pid) {
            let expected_cc = (last_cc + 1) & 0x0F;
            if current_cc != expected_cc {
                log::error!(
                    "PidTracker: ({}) Continuity error for PID {}: expected {} but got {}",
                    label,
                    pid,
                    expected_cc,
                    current_cc
                );
                // Update stored CC to the new value, to avoid compounding errors.
                self.continuity.insert(pid, current_cc);
                return Err(pid);
            }
        }

        // Update with the latest CC.
        self.continuity.insert(pid, current_cc);

        Ok(())
    }
}
