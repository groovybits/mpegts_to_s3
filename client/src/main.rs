use anyhow::{anyhow, Result};
use clap::{Arg, ArgAction, Command as ClapCommand};
use m3u8_rs::{parse_playlist_res, Playlist};
use reqwest::blocking::Client;
use std::collections::{HashSet, VecDeque};
use std::net::UdpSocket;
use std::thread::sleep;
use std::time::Duration;
use url::Url;

const TS_PACKET_SIZE: usize = 188;
const CHUNK_SIZE: usize = 7 * TS_PACKET_SIZE; // 1316 bytes

// Track segments we've processed
struct SegmentHistory {
    seen: HashSet<String>,
    queue: VecDeque<String>,
    capacity: usize,
}

impl SegmentHistory {
    fn new(cap: usize) -> Self {
        Self {
            seen: HashSet::new(),
            queue: VecDeque::new(),
            capacity: cap,
        }
    }

    fn contains(&self, uri: &str) -> bool {
        self.seen.contains(uri)
    }

    fn insert(&mut self, uri: String) {
        self.seen.insert(uri.clone());
        self.queue.push_back(uri);
        while self.queue.len() > self.capacity {
            if let Some(old) = self.queue.pop_front() {
                self.seen.remove(&old);
            }
        }
    }
}

struct TsChunk {
    data: Vec<u8>,
    pcr_27mhz: Option<u64>,
}

/// found a previous PCR for pacing, store it here.
fn send_ts_chunks_realtime(
    sock: &UdpSocket,
    dst_addr: &str, // e.g. "224.0.0.200:10001"
    chunks: &[TsChunk],
    prev_pcr: &mut Option<u64>,
) -> Result<()> {
    for chunk in chunks {
        // If we have a new PCR, sleep based on difference
        if let (Some(cur_pcr), Some(old_pcr)) = (chunk.pcr_27mhz, *prev_pcr) {
            if cur_pcr > old_pcr {
                let diff = cur_pcr - old_pcr;
                let diff_secs = diff as f64 / 27_000_000.0;
                if diff_secs > 0.0 && diff_secs < 10.0 {
                    sleep(Duration::from_secs_f64(diff_secs));
                }
            }
        }

        // Send to the multicast address
        sock.send_to(&chunk.data, dst_addr)?;

        // Update previous PCR
        if let Some(cur_pcr) = chunk.pcr_27mhz {
            *prev_pcr = Some(cur_pcr);
        }
    }
    Ok(())
}

/// Minimal parse of PCR from the last TS packet in a chunk, etc.
fn parse_pcr_from_ts_packet(packet: &[u8]) -> Option<u64> {
    if packet.len() != TS_PACKET_SIZE || packet[0] != 0x47 {
        return None;
    }
    let afc = (packet[3] >> 4) & 0b11; // adaptation_field_control
    if afc == 0b01 {
        // payload only, no adaptation
        return None;
    }
    let adapt_len = packet[4] as usize;
    if adapt_len == 0 || adapt_len + 5 > packet.len() {
        return None;
    }
    let flags = packet[5];
    if (flags & 0x10) == 0 {
        return None;
    }
    // read pcr from packet[6..12]
    if 6 + 6 > packet.len() {
        return None;
    }
    let pcr_data = &packet[6..12];
    let pcr_base = ((pcr_data[0] as u64) << 25)
        | ((pcr_data[1] as u64) << 17)
        | ((pcr_data[2] as u64) << 9)
        | ((pcr_data[3] as u64) << 1)
        | ((pcr_data[4] as u64) >> 7);
    let pcr_ext = (((pcr_data[4] & 0x01) as u64) << 8) | (pcr_data[5] as u64);
    Some(pcr_base * 300 + pcr_ext)
}

fn parse_ts_segment_into_chunks(data: &[u8]) -> Vec<TsChunk> {
    let mut out = Vec::new();
    let mut offset = 0;

    while offset < data.len() {
        let end = (offset + CHUNK_SIZE).min(data.len());
        let chunk_buf = &data[offset..end];
        offset = end;

        let pcr_27mhz = if chunk_buf.len() >= TS_PACKET_SIZE {
            // parse PCR from the last 188 bytes
            let start_last_pkt = chunk_buf.len() - TS_PACKET_SIZE;
            parse_pcr_from_ts_packet(&chunk_buf[start_last_pkt..])
        } else {
            None
        };

        out.push(TsChunk {
            data: chunk_buf.to_vec(),
            pcr_27mhz,
        });
    }
    out
}

/// Return a Url from base + relative path
fn resolve_segment_url(base: &Url, seg_path: &str) -> Result<Url> {
    base.join(seg_path)
        .map_err(|e| anyhow!("Bad URL join: {}", e))
}

fn main() -> Result<()> {
    let matches = ClapCommand::new("hls-to-udp")
        .version("1.0")
        .about("Fetch TS segments, chunk them, parse PCR, send over UDP.")
        .arg(
            Arg::new("m3u8_url")
                .long("m3u8-url")
                .short('u')
                .action(ArgAction::Set)
                .required(true),
        )
        .arg(
            Arg::new("udp_output")
                .long("udp-output")
                .short('o')
                .action(ArgAction::Set)
                .required(true)
                .help("e.g., 224.0.0.200:10001 (no udp:// prefix)"),
        )
        .arg(
            Arg::new("interval")
                .long("interval")
                .short('i')
                .action(ArgAction::Set)
                .default_value("2")
                .help("Seconds to wait before re-polling the playlist"),
        )
        .arg(
            Arg::new("history_size")
                .long("history-size")
                .short('s')
                .action(ArgAction::Set)
                .default_value("2000"),
        )
        .get_matches();

    let m3u8_url = matches.get_one::<String>("m3u8_url").unwrap();
    let udp_addr = matches.get_one::<String>("udp_output").unwrap();
    let interval_s = matches
        .get_one::<String>("interval")
        .unwrap()
        .parse::<u64>()
        .unwrap_or(2);
    let history_capacity = matches
        .get_one::<String>("history_size")
        .unwrap()
        .parse::<usize>()
        .unwrap_or(2000);

    println!("Starting HLS->UDP (PCR-based) with:");
    println!("  M3U8 URL   = {}", m3u8_url);
    println!("  UDP output = {}", udp_addr);
    println!("  Interval   = {}s", interval_s);
    println!("  History    = {}", history_capacity);

    // Setup
    let base_url = Url::parse(m3u8_url)?;
    let client = Client::new();
    let mut seg_history = SegmentHistory::new(history_capacity);

    // Create a UDP socket, but do NOT connect for multicast
    // We'll do sock.send_to(...).
    let sock = UdpSocket::bind("0.0.0.0:0")?;

    // to configure TTL or loopback for multicast:
    // sock.set_multicast_loop_v4(false)?;
    // sock.set_multicast_ttl_v4(2)?;

    // join the group for receiving, you'd do:
    // sock.join_multicast_v4(&"224.0.0.200".parse()?, &"0.0.0.0".parse()?)?;

    // Keep track of the last PCR across segments
    let mut prev_pcr: Option<u64> = None;

    loop {
        // 1) Fetch M3U8
        let playlist_text = match client.get(m3u8_url).send() {
            Ok(r) => {
                if !r.status().is_success() {
                    eprintln!("Fetch error: status={}", r.status());
                    sleep(Duration::from_secs(5));
                    continue;
                }
                r.text()?
            }
            Err(e) => {
                eprintln!("HTTP error: {}", e);
                sleep(Duration::from_secs(5));
                continue;
            }
        };

        // 2) Parse
        let parsed = parse_playlist_res(playlist_text.as_bytes());
        let media_playlist = match parsed {
            Ok(Playlist::MediaPlaylist(mp)) => mp,
            Ok(_) => {
                eprintln!("Master or invalid playlist. Retrying...");
                sleep(Duration::from_secs(5));
                continue;
            }
            Err(e) => {
                eprintln!("Parse error: {:?} => retry...", e);
                sleep(Duration::from_secs(5));
                continue;
            }
        };

        // 3) For each new segment
        for seg in &media_playlist.segments {
            let uri = &seg.uri;
            if seg_history.contains(uri) {
                continue;
            }
            seg_history.insert(uri.to_string());

            let seg_url = resolve_segment_url(&base_url, uri)?;
            println!("Downloading new segment: {}", seg_url);

            let seg_data = client.get(seg_url).send()?.bytes()?;
            let chunks = parse_ts_segment_into_chunks(&seg_data);

            // Send chunk-by-chunk, pacing via PCR
            send_ts_chunks_realtime(&sock, udp_addr, &chunks, &mut prev_pcr)?;
        }

        if media_playlist.end_list {
            println!("ENDLIST found => done (VOD).");
            break;
        }

        // Wait a bit
        sleep(Duration::from_secs(interval_s));
    }

    Ok(())
}
