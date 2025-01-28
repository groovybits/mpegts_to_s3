use anyhow::{anyhow, Result};
use clap::{Arg, ArgAction, Command as ClapCommand};
use m3u8_rs::{parse_playlist_res, Playlist};
use reqwest::blocking::Client;
use std::collections::{HashSet, VecDeque};
use std::net::UdpSocket;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, sleep, JoinHandle};
use std::time::{Duration, Instant};
use url::Url;

const TS_PACKET_SIZE: usize = 188;
const MAX_UDP_BITRATE: f64 = 30_000_000.0; // 30 Mbps max
const UDP_RETRY_DELAY: Duration = Duration::from_millis(10);

#[derive(Debug)]
struct DownloadedSegment {
    id: usize,
    data: Vec<u8>,
    duration: f64,
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

fn resolve_segment_url(base: &Url, seg_path: &str) -> Result<Url> {
    base.join(seg_path)
        .map_err(|e| anyhow!("Failed URL join: {}", e))
}

fn parse_pcr(ts_packet: &[u8]) -> Option<u64> {
    if ts_packet.len() != TS_PACKET_SIZE || ts_packet[0] != 0x47 {
        return None;
    }
    let afc = (ts_packet[3] >> 4) & 0b11;
    if afc == 0b00 || afc == 0b01 {
        return None;
    }
    let adapt_len = ts_packet[4] as usize;
    if adapt_len == 0 || (adapt_len + 5) > ts_packet.len() {
        return None;
    }
    let flags = ts_packet[5];
    if (flags & 0x10) == 0 {
        return None;
    }
    if adapt_len < 7 {
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
    let pcr_ext = (((p[4] & 0x01) as u64) << 8) | (p[5] as u64);

    let total_90k = pcr_base * 300 + pcr_ext;
    Some(total_90k)
}

struct BitrateEstimator {
    average_bitrate: f64,
    alpha: f64,
}

impl BitrateEstimator {
    fn new() -> Self {
        Self {
            average_bitrate: 4_000_000.0,
            alpha: 0.2,
        }
    }

    fn update(&mut self, bytes: usize, duration: f64) {
        if duration > 0.0 {
            let bitrate = (bytes as f64 * 8.0) / duration;
            self.average_bitrate = self.alpha * bitrate + (1.0 - self.alpha) * self.average_bitrate;
        }
    }

    fn current_bitrate(&self) -> f64 {
        self.average_bitrate.min(MAX_UDP_BITRATE)
    }
}

fn send_segment(
    sock: &UdpSocket,
    segment_data: &[u8],
    last_pcr: &mut Option<u64>,
    segment_duration: f64,
    current_bitrate: f64,
) -> Result<()> {
    let num_packets = segment_data.len() / TS_PACKET_SIZE;
    let mut any_pcr = false;

    let mut offset_check = 0;
    while offset_check + TS_PACKET_SIZE <= segment_data.len() {
        let packet = &segment_data[offset_check..offset_check + TS_PACKET_SIZE];
        if parse_pcr(packet).is_some() {
            any_pcr = true;
            break;
        }
        offset_check += TS_PACKET_SIZE;
    }

    if any_pcr {
        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= segment_data.len() {
            let packet = &segment_data[offset..offset + TS_PACKET_SIZE];
            offset += TS_PACKET_SIZE;

            if let Some(cur) = parse_pcr(packet) {
                if let Some(prev) = *last_pcr {
                    if cur > prev {
                        let diff = cur - prev;
                        let diff_secs = diff as f64 / 90_000.0;
                        if diff_secs > 0.0 && diff_secs < 10.0 {
                            sleep(Duration::from_secs_f64(diff_secs));
                        }
                    }
                }
                *last_pcr = Some(cur);
            }

            loop {
                match sock.send(packet) {
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        sleep(UDP_RETRY_DELAY);
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
    } else if segment_duration > 0.0 && num_packets > 0 {
        let calculated_duration =
            segment_duration.max((segment_data.len() as f64 * 8.0) / current_bitrate);

        let time_per_packet = calculated_duration / num_packets as f64;
        let start_time = Instant::now();
        let mut offset = 0;

        for i in 0..num_packets {
            let target_time = start_time + Duration::from_secs_f64(i as f64 * time_per_packet);

            let packet = &segment_data[offset..offset + TS_PACKET_SIZE];
            loop {
                match sock.send(packet) {
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        sleep(UDP_RETRY_DELAY);
                    }
                    Err(e) => return Err(e.into()),
                }
            }

            offset += TS_PACKET_SIZE;

            let now = Instant::now();
            if let Some(remaining) = target_time.checked_duration_since(now) {
                sleep(remaining);
            }
        }
    } else {
        let packet_size_bits = (TS_PACKET_SIZE * 8) as f64;
        let interval = packet_size_bits / current_bitrate;
        let mut offset = 0;

        while offset < segment_data.len() {
            let send_start = Instant::now();
            let packet = &segment_data[offset..offset + TS_PACKET_SIZE];

            loop {
                match sock.send(packet) {
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        sleep(UDP_RETRY_DELAY);
                    }
                    Err(e) => return Err(e.into()),
                }
            }

            offset += TS_PACKET_SIZE;

            let elapsed = send_start.elapsed();
            let target_duration = Duration::from_secs_f64(interval);
            if let Some(remaining) = target_duration.checked_sub(elapsed) {
                sleep(remaining);
            }
        }
    }

    Ok(())
}

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
                    duration: seg.duration.into(),
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

fn sender_thread(udp_addr: String, rx: Receiver<DownloadedSegment>) -> JoinHandle<()> {
    thread::spawn(move || {
        let sock = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => {
                eprintln!("UDP bind error: {}", e);
                return;
            }
        };

        if let Err(e) = sock.connect(&udp_addr) {
            eprintln!("Failed to connect UDP socket: {}", e);
            return;
        }

        let mut last_pcr: Option<u64> = None;
        let mut bitrate_estimator = BitrateEstimator::new();

        while let Ok(seg) = rx.recv() {
            println!("Sender processing segment #{}", seg.id);
            bitrate_estimator.update(seg.data.len(), seg.duration);

            if let Err(e) = send_segment(
                &sock,
                &seg.data,
                &mut last_pcr,
                seg.duration,
                bitrate_estimator.current_bitrate(),
            ) {
                eprintln!("Critical send error: {}", e);
                break;
            }
        }
        println!("Sender thread exiting.");
    })
}

fn get_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn main() -> Result<()> {
    let matches = ClapCommand::new("hls-udp-streamer")
        .version(get_version())
        .about("HLS to UDP streamer with PCR/duration-based timing")
        .arg(
            Arg::new("m3u8_url")
                .short('u')
                .long("m3u8-url")
                .required(true)
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("udp_output")
                .short('o')
                .long("udp-output")
                .required(true)
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("poll_ms")
                .short('p')
                .long("poll-ms")
                .default_value("100")
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("history_size")
                .short('s')
                .long("history-size")
                .default_value("2000")
                .action(ArgAction::Set),
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

    println!("Starting streamer:");
    println!("  M3U8 URL: {}", m3u8_url);
    println!("  UDP Target: {}", udp_out);
    println!("  Poll Interval: {} ms", poll_ms);
    println!("  History Size: {}", hist_cap);

    let (tx, rx) = mpsc::channel();
    let dl_handle = downloader_thread(m3u8_url, poll_ms, hist_cap, tx);
    let sender_handle = sender_thread(udp_out, rx);

    dl_handle.join().ok();
    sender_handle.join().ok();

    println!("Main thread exiting.");
    Ok(())
}
