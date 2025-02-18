use std::collections::HashMap;

pub const TS_PACKET_SIZE: usize = 188;
pub const PID_NULL: u16 = 0x1FFF;

pub struct PidTracker {
    continuity: HashMap<u16, u8>,
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn test_pid_tracker() {
        let mut tracker = PidTracker::new();
        let label = String::from("test.ts");

        // Try to open "test.ts". If it doesn't exist, use a basic MPEG-TS packet blob.
        if let Ok(mut file) = std::fs::File::open("test.ts") {
            let mut packet = [0u8; TS_PACKET_SIZE];
            loop {
                match file.read_exact(&mut packet) {
                    Ok(_) => {
                        if let Err(pid) = tracker.process_packet(label.clone(), &packet) {
                            println!("Error with PID: {}", pid);
                        }
                    }
                    Err(_) => break,
                }
            }
        } else {
            // Create a basic MPEG-TS packet blob with one packet and one PID.
            // Construct a valid TS packet:
            // Byte 0: Sync byte (0x47).
            // Bytes 1-2: Set PID to 100 (0x0064). Since 100 < 256, upper 5 bits in byte 1 are 0,
            // and byte 2 is 0x64.
            // Byte 3: Adaptation field control (payload only = 0b0001 shifted left 4) and continuity counter 0.
            // The remainder of the packet is zeroed.
            let mut packet = [0u8; TS_PACKET_SIZE];
            packet[0] = 0x47; // Sync byte.
            packet[1] = 0x00; // Upper PID bits and flags.
            packet[2] = 100; // Lower PID bits.
            packet[3] = 0x10; // Adaptation field control = 0b0001 << 4; continuity counter = 0.

            // Process the generated packet.
            if let Err(pid) = tracker.process_packet(label, &packet) {
                println!("Error with PID: {}", pid);
            } else {
                println!("Processed generated packet successfully.");
            }
        }
    }
}
