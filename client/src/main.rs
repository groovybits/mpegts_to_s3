use anyhow::{anyhow, Result};
use clap::{Arg, ArgAction, Command as ClapCommand};
use m3u8_rs::{parse_playlist_res, Playlist};
use reqwest::blocking::Client;
use std::collections::{HashSet, VecDeque};
use std::net::UdpSocket;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, sleep, JoinHandle};
use std::time::Duration;
use url::Url;

const TS_PACKET_SIZE: usize = 188;

/// Holds the full segment data plus a "segment ID" for debugging
#[derive(Debug)]
struct DownloadedSegment {
    id:   usize,
    data: Vec<u8>,
}

/// Track which segments we've seen
struct SegmentHistory {
    seen: HashSet<String>,
    queue: VecDeque<String>,
    capacity: usize,
}

impl SegmentHistory {
    fn new(capacity: usize) -> Self {
        Self {
            seen: HashSet::new(),
            queue: VecDeque::new(),
            capacity,
        }
    }

    fn contains(&self, uri: &str) -> bool {
        self.seen.contains(uri)
    }

    fn insert(&mut self, uri: String) {
        self.seen.insert(uri.clone());
        self.queue.push_back(uri);
        while self.queue.len() > self.capacity {
            if let Some(rem) = self.queue.pop_front() {
                self.seen.remove(&rem);
            }
        }
    }
}

/// Convert HLS relative path to absolute URL
fn resolve_segment_url(base: &Url, seg_path: &str) -> Result<Url> {
    base.join(seg_path).map_err(|e| anyhow!("Failed URL join: {}", e))
}

/// Minimal parse of PCR from a 188-byte TS packet's adaptation field, if present.
fn parse_pcr(ts_packet: &[u8]) -> Option<u64> {
    if ts_packet.len() != TS_PACKET_SIZE || ts_packet[0] != 0x47 {
        return None;
    }
    // Byte 3 top bits => adaptation_field_control
    let afc = (ts_packet[3] >> 4) & 0b11;
    if afc == 0b01 {
        // payload only, no adaptation => no PCR
        return None;
    }
    let adapt_len = ts_packet[4] as usize;
    if adapt_len == 0 || (adapt_len + 5) > ts_packet.len() {
        return None;
    }
    let flags = ts_packet[5];
    if (flags & 0x10) == 0 {
        // pcr_flag not set
        return None;
    }
    // pcr is 6 bytes at [6..12]
    if 6 + 6 > ts_packet.len() {
        return None;
    }
    let p = &ts_packet[6..12];
    let pcr_base = ((p[0] as u64) << 25)
        | ((p[1] as u64) << 17)
        | ((p[2] as u64) << 9)
        | ((p[3] as u64) << 1)
        | ((p[4] as u64) >> 7);
    let pcr_ext = (((p[4] & 0x01) as u64) << 8) | (p[5] as u64);
    Some(pcr_base * 300 + pcr_ext)
}

/// This function sends the entire segment, packet by packet, sleeping by PCR diffs.
fn send_segment_by_pcr(
    sock: &UdpSocket,
    dst_addr: &str,
    segment_data: &[u8],
    last_pcr: &mut Option<u64>,
) -> Result<()> {
    let mut offset = 0;
    while offset + TS_PACKET_SIZE <= segment_data.len() {
        let packet = &segment_data[offset..offset + TS_PACKET_SIZE];
        offset += TS_PACKET_SIZE;

        if let Some(cur) = parse_pcr(packet) {
            // if we have a previous
            if let Some(prev) = *last_pcr {
                if cur > prev {
                    let diff = cur - prev;
                    // 27,000,000 ticks = 1 sec
                    let diff_secs = diff as f64 / 27_000_000.0;
                    // Sleep if < 10s or so
                    if diff_secs > 0.0 && diff_secs < 10.0 {
                        sleep(Duration::from_secs_f64(diff_secs));
                    }
                } else {
                    // negative => discontinuity => do not sleep
                }
            }
            *last_pcr = Some(cur);
        }

        // Send the packet
        sock.send_to(packet, dst_addr)?;
    }
    Ok(())
}

/// Thread that downloads new segments (frequent M3U8 checks) and sends them to a channel.
fn downloader_thread(
    m3u8_url: String,
    poll_interval_ms: u64,
    hist_capacity: usize,
    tx: Sender<DownloadedSegment>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let client = Client::new();
        let base_url = match Url::parse(&m3u8_url) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("Invalid M3U8 URL: {}", e);
                return;
            }
        };
        let mut seg_history = SegmentHistory::new(hist_capacity);
        let mut next_seg_id: usize = 1;

        loop {
            // 1) Fetch playlist
            let playlist_text = match client.get(&m3u8_url).send() {
                Ok(r) => {
                    if !r.status().is_success() {
                        eprintln!("M3U8 fetch HTTP error: {}", r.status());
                        thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                    match r.text() {
                        Ok(txt) => txt,
                        Err(e) => {
                            eprintln!("M3U8 read error: {}", e);
                            thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("M3U8 request error: {}", e);
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }
            };

            // 2) Parse
            let parsed = m3u8_rs::parse_playlist_res(playlist_text.as_bytes());
            let media_pl = match parsed {
                Ok(Playlist::MediaPlaylist(mp)) => mp,
                Ok(_) => {
                    eprintln!("Got Master/unknown playlist, ignoring...");
                    thread::sleep(Duration::from_millis(poll_interval_ms));
                    continue;
                }
                Err(e) => {
                    eprintln!("Parse error: {:?}, ignoring...", e);
                    thread::sleep(Duration::from_millis(poll_interval_ms));
                    continue;
                }
            };

            // 3) Download new segments
            for seg in &media_pl.segments {
                let uri = &seg.uri;
                if seg_history.contains(uri) {
                    continue;
                }
                seg_history.insert(uri.to_string());

                // Download
                let seg_url = match resolve_segment_url(&base_url, uri) {
                    Ok(u) => u,
                    Err(e) => {
                        eprintln!("Bad segment URL: {}", e);
                        continue;
                    }
                };
                println!("Downloading segment {} => {}", next_seg_id, seg_url);

                let seg_bytes = match client.get(seg_url).send() {
                    Ok(resp) => {
                        if !resp.status().is_success() {
                            eprintln!("Segment fetch error: {}", resp.status());
                            continue;
                        }
                        match resp.bytes() {
                            Ok(b) => b.to_vec(),
                            Err(e) => {
                                eprintln!("Segment read err: {}", e);
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Segment request error: {}", e);
                        continue;
                    }
                };

                // Push into channel
                let seg_struct = DownloadedSegment {
                    id: next_seg_id,
                    data: seg_bytes,
                };
                if tx.send(seg_struct).is_err() {
                    eprintln!("Receiver dropped, downloader exiting.");
                    return;
                }
                next_seg_id += 1;
            }

            // 4) If it's VOD (#EXT-X-ENDLIST), we can optionally break
            if media_pl.end_list {
                println!("ENDLIST found => done downloading.");
                break;
            }

            // Wait sub-second or user-defined ms
            thread::sleep(Duration::from_millis(poll_interval_ms));
        }
    })
}

/// Thread that reads from the channel, and sends each segment's TS packets by PCR over UDP.
fn sender_thread(
    udp_addr: String,
    rx: Receiver<DownloadedSegment>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        // Make a UDP socket
        let sock = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => {
                eprintln!("UDP bind error: {}", e);
                return;
            }
        };
        // If needed: sock.set_multicast_ttl_v4(2).ok();
        // Keep track of lastPCR
        let mut last_pcr: Option<u64> = None;

        while let Ok(seg) = rx.recv() {
            println!("Sender got segment #{}", seg.id);
            // Send it, packet by packet
            if let Err(e) = send_segment_by_pcr(&sock, &udp_addr, &seg.data, &mut last_pcr) {
                eprintln!("Send error: {}", e);
                break;
            }
        }

        println!("Sender done (channel closed).");
    })
}

fn main() -> Result<()> {
    let matches = ClapCommand::new("hls-udp-two-threads")
        .version("1.0")
        .about("Download HLS segments in one thread, feed them out by PCR in another, sub-second poll.")
        .arg(
            Arg::new("m3u8_url")
                .short('u')
                .long("m3u8-url")
                .action(ArgAction::Set)
                .required(true)
                .help("URL to your .m3u8"),
        )
        .arg(
            Arg::new("udp_output")
                .short('o')
                .long("udp-output")
                .action(ArgAction::Set)
                .required(true)
                .help("like 224.0.0.200:10001 (no udp:// prefix)"),
        )
        .arg(
            Arg::new("poll_ms")
                .short('p')
                .long("poll-ms")
                .action(ArgAction::Set)
                .default_value("500")
                .help("How many ms between M3U8 fetch (default=500)"),
        )
        .arg(
            Arg::new("history_size")
                .short('s')
                .long("history-size")
                .action(ArgAction::Set)
                .default_value("2000"),
        )
        .get_matches();

    let m3u8_url = matches.get_one::<String>("m3u8_url").unwrap().clone();
    let udp_out = matches.get_one::<String>("udp_output").unwrap().clone();
    let poll_ms = matches
        .get_one::<String>("poll_ms")
        .unwrap()
        .parse::<u64>()
        .unwrap_or(500);
    let hist_cap = matches
        .get_one::<String>("history_size")
        .unwrap()
        .parse::<usize>()
        .unwrap_or(2000);

    println!("Starting with:");
    println!("  M3U8 URL     = {}", m3u8_url);
    println!("  UDP Output   = {}", udp_out);
    println!("  Poll ms      = {}", poll_ms);
    println!("  History size = {}", hist_cap);

    // Channel between downloader -> sender
    let (tx, rx) = mpsc::channel::<DownloadedSegment>();

    // Spawn threads
    let dl_handle = downloader_thread(m3u8_url, poll_ms, hist_cap, tx);
    let sender_handle = sender_thread(udp_out, rx);

    // Wait
    dl_handle.join().ok();
    sender_handle.join().ok();

    println!("Main done.");
    Ok(())
}
