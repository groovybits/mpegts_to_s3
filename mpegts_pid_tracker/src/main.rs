use mpegts_pid_tracker::PidTracker;
use mpegts_pid_tracker::TS_PACKET_SIZE;
use std::io::Read;

// Main function showing basic usage with input from a file or stdin.
fn main() {
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
