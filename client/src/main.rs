use anyhow::{anyhow, Result};
use clap::{Arg, ArgAction, Command as ClapCommand};
use m3u8_rs::{parse_playlist_res, Playlist};
use reqwest::blocking::Client;
use std::collections::{HashSet, VecDeque};
use std::net::UdpSocket;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, sleep, JoinHandle};
use std::time::Duration;
use url::Url;

const TS_PACKET_SIZE: usize = 188;

// ----------------------------------------------------------------
// 1) The "DownloadedSegment" and "SegmentHistory"
// ----------------------------------------------------------------
#[derive(Debug)]
struct DownloadedSegment {
    id:   usize,
    data: Vec<u8>,
}

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

// ----------------------------------------------------------------
// 2) URL helper
// ----------------------------------------------------------------
fn resolve_segment_url(base: &Url, seg_path: &str) -> Result<Url> {
    base.join(seg_path)
        .map_err(|e| anyhow!("Failed URL join: {}", e))
}

// ----------------------------------------------------------------
// 3) The PCR parser (for 90 kHz TS).
// ----------------------------------------------------------------
fn parse_pcr(ts_packet: &[u8]) -> Option<u64> {
    if ts_packet.len() != TS_PACKET_SIZE || ts_packet[0] != 0x47 {
        return None;
    }
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
    if 6 + 6 > ts_packet.len() {
        return None;
    }

    let p = &ts_packet[6..12];
    let pcr_base = ((p[0] as u64) << 25)
                 | ((p[1] as u64) << 17)
                 | ((p[2] as u64) << 9)
                 | ((p[3] as u64) << 1)
                 | ((p[4] as u64) >> 7);
    let pcr_ext  = (((p[4] & 0x01) as u64) << 8) | (p[5] as u64);

    // treat 'pcr_base' as 90 kHz, plus the extension fraction:
    //
    // total  = pcr_base + (pcr_ext / 300.0)
    //
    // use an integer in "pcr_base*300 + pcr_ext"
    let total_90k = pcr_base * 300 + pcr_ext;
    Some(total_90k) 
}

// ----------------------------------------------------------------
// 4) "sender" logic: We interpret the difference as 90k => seconds
// ----------------------------------------------------------------
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
            if let Some(prev) = *last_pcr {
                if cur > prev {
                    let diff = cur - prev;
                    // Now we interpret 'diff' as a difference in "ticks".
                    // If the TS is truly 90 kHz for base + extension => 27MHz, 
                    let diff_secs = diff as f64 / 90_000.0;  
                    if diff_secs > 0.0 && diff_secs < 10.0 {
                        // eprintln!("sleeping for {:.6}", diff_secs); // Debug
                        sleep(Duration::from_secs_f64(diff_secs));
                    }
                }
            }
            *last_pcr = Some(cur);
        }

        sock.send_to(packet, dst_addr)?;
    }
    Ok(())
}

// ----------------------------------------------------------------
// 5) downloader thread
// ----------------------------------------------------------------
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
            // 1) fetch M3U8
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

            let parsed = parse_playlist_res(playlist_text.as_bytes());
            let media_pl = match parsed {
                Ok(Playlist::MediaPlaylist(mp)) => mp,
                Ok(_) => {
                    eprintln!("Got Master/unknown playlist...");
                    thread::sleep(Duration::from_millis(poll_interval_ms));
                    continue;
                }
                Err(e) => {
                    eprintln!("Parse error: {:?}, ignoring...", e);
                    thread::sleep(Duration::from_millis(poll_interval_ms));
                    continue;
                }
            };

            // 2) download new segments
            for seg in &media_pl.segments {
                let uri = &seg.uri;
                if seg_history.contains(uri) {
                    continue;
                }
                seg_history.insert(uri.to_string());

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

            if media_pl.end_list {
                println!("ENDLIST found => done downloading.");
                break;
            }

            thread::sleep(Duration::from_millis(poll_interval_ms));
        }
    })
}

// ----------------------------------------------------------------
// 6) The sender thread
// ----------------------------------------------------------------
fn sender_thread(
    udp_addr: String,
    rx: Receiver<DownloadedSegment>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let sock = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => {
                eprintln!("UDP bind error: {}", e);
                return;
            }
        };

        let mut last_pcr: Option<u64> = None;

        while let Ok(seg) = rx.recv() {
            println!("Sender got segment #{}", seg.id);
            if let Err(e) = send_segment_by_pcr(&sock, &udp_addr, &seg.data, &mut last_pcr) {
                eprintln!("Send error: {}", e);
                break;
            }
        }
        println!("Sender done (channel closed).");
    })
}

// ----------------------------------------------------------------
// 7) Main
// ----------------------------------------------------------------
fn main() -> Result<()> {
    let matches = ClapCommand::new("hls-udp-two-threads")
        .version("1.0")
        .about("Download HLS segments, parse TS PCR at 90k, feed over UDP.")
        .arg(
            Arg::new("m3u8_url")
                .short('u')
                .long("m3u8-url")
                .action(ArgAction::Set)
                .required(true)
        )
        .arg(
            Arg::new("udp_output")
                .short('o')
                .long("udp-output")
                .action(ArgAction::Set)
                .required(true)
        )
        .arg(
            Arg::new("poll_ms")
                .short('p')
                .long("poll-ms")
                .action(ArgAction::Set)
                .default_value("500")
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

    let (tx, rx) = mpsc::channel::<DownloadedSegment>();

    let dl_handle = downloader_thread(m3u8_url, poll_ms, hist_cap, tx);
    let sender_handle = sender_thread(udp_out, rx);

    dl_handle.join().ok();
    sender_handle.join().ok();

    println!("Main done.");
    Ok(())
}
