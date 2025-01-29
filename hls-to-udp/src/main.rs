use anyhow::{anyhow, Result};
use clap::{Arg, ArgAction, Command as ClapCommand};
use m3u8_rs::{parse_playlist_res, Playlist};
use reqwest::blocking::Client;
use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::net::UdpSocket;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, sleep, JoinHandle};
use std::time::Duration;
use url::Url;
//use libltntstools_sys::*;
use libltntstools::PcrSmoother;

use env_logger;

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

        let callback = |v: Vec<u8>| {
            log::debug!(
                "Callback received buffer with {} bytes, sending to UDP.",
                v.len()
            );
            loop {
                match sock.send(v.as_slice()) {
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        sleep(UDP_RETRY_DELAY);
                    }
                    Err(e) => {
                        eprintln!("UDP send error: {}", e);
                        break;
                    }
                }
            }
        };
        let pcr_pid = 0x31;
        let bitrate = 20000;
        let pkt_size = 1316;
        let latency_ms = 50;
        let mut smoother = PcrSmoother::new(pcr_pid, bitrate, pkt_size, latency_ms, callback);

        while let Ok(seg) = rx.recv() {
            log::debug!(
                "Sender processing segment #{} of {}bytes and {}s duration",
                seg.id,
                seg.data.len(),
                seg.duration
            );
            let _ = smoother.write(&seg.data);
        }
        println!("Sender thread exiting.");
    })
}

fn get_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn main() -> Result<()> {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .format_timestamp_secs()
        .init();
    log::info!("HLStoUDP: Logging initialized. Starting main()...");
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
                .default_value("500")
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("history_size")
                .short('s')
                .long("history-size")
                .default_value("32")
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
        .unwrap_or(32);

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
