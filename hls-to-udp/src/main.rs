use anyhow::{anyhow, Result};
use clap::{Arg, ArgAction, Command as ClapCommand};
use env_logger;
use libltntstools::{PcrSmoother, StreamModel};
use m3u8_rs::{parse_playlist_res, Playlist};
use reqwest::blocking::Client;
use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::net::UdpSocket;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, sleep, JoinHandle};
use std::time::Duration;
use url::Url;

const UDP_RETRY_DELAY: Duration = Duration::from_millis(1);

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
        .map_err(|e| anyhow!("ResolveSegmentUrl: Failed URL join: {}", e))
}

fn receiver_thread(
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
                eprintln!("ReceiverThread: Invalid M3U8 URL: {}", e);
                return;
            }
        };
        let mut seg_history = SegmentHistory::new(hist_capacity);
        let mut next_seg_id: usize = 1;

        loop {
            let playlist_text = match client.get(&m3u8_url).send() {
                Ok(r) => {
                    if !r.status().is_success() {
                        eprintln!("ReceiverThread: 3U8 fetch HTTP error: {}", r.status());
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    match r.text() {
                        Ok(txt) => txt,
                        Err(e) => {
                            eprintln!("ReceiverThread: M3U8 read error: {}", e);
                            thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("ReceiverThread: M3U8 request error: {}", e);
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };

            let parsed = parse_playlist_res(playlist_text.as_bytes());
            let media_pl = match parsed {
                Ok(Playlist::MediaPlaylist(mp)) => mp,
                Ok(_) => {
                    eprintln!("ReceiverThread: Got Master/unknown playlist...");
                    thread::sleep(Duration::from_millis(poll_interval_ms));
                    continue;
                }
                Err(e) => {
                    eprintln!("ReceiverThread: Parse error: {:?}, ignoring...", e);
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
                        eprintln!("ReceiverThread: Bad segment URL: {}", e);
                        continue;
                    }
                };
                println!(
                    "ReceiverThread: Downloading segment {} => {}",
                    next_seg_id, seg_url
                );

                let seg_bytes = match client.get(seg_url).send() {
                    Ok(resp) => {
                        if !resp.status().is_success() {
                            eprintln!("ReceiverThread: Segment fetch error: {}", resp.status());
                            continue;
                        }
                        match resp.bytes() {
                            Ok(b) => b.to_vec(),
                            Err(e) => {
                                eprintln!("ReceiverThread: Segment read err: {}", e);
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("ReceiverThread: Segment request error: {}", e);
                        continue;
                    }
                };

                let seg_struct = DownloadedSegment {
                    id: next_seg_id,
                    data: seg_bytes,
                    duration: seg.duration.into(),
                };
                if tx.send(seg_struct).is_err() {
                    eprintln!("ReceiverThread: Receiver dropped, downloader exiting.");
                    return;
                }
                next_seg_id += 1;
            }

            if media_pl.end_list {
                println!("ReceiverThread: ENDLIST found => done downloading.");
                break;
            }

            thread::sleep(Duration::from_millis(poll_interval_ms));
        }
    })
}

fn sender_thread(
    udp_addr: String,
    latency: i32,
    pcr_pid_arg: u16,
    pkt_size: i32,
    rate: i32,
    rx: Receiver<DownloadedSegment>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut pcr_pid = pcr_pid_arg;
        let sock = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => {
                eprintln!("SenderThread: UDP bind error: {}", e);
                return;
            }
        };

        // Set non-blocking mode to prevent send from blocking
        if let Err(e) = sock.set_nonblocking(true) {
            eprintln!("SenderThread: Failed to set non-blocking socket: {}", e);
            return;
        }

        if let Err(e) = sock.connect(&udp_addr) {
            eprintln!("SenderThread: Failed to connect UDP socket: {}", e);
            return;
        }

        let smoother_callback = |v: Vec<u8>| {
            log::debug!(
                "SmootherCallback: received buffer with {} bytes, sending to UDP.",
                v.len()
            );
            let mut retries = 0;
            let max_retries = 5;
            loop {
                match sock.send(v.as_slice()) {
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        retries += 1;
                        if retries >= max_retries {
                            log::error!("SmootherCallback: Max retries reached, dropping packet.");
                            break;
                        }
                        sleep(UDP_RETRY_DELAY);
                    }
                    Err(e) => {
                        eprintln!("SmootherCallback: UDP send error: {}", e);
                        break;
                    }
                }
            }
        };
        // setup channels to communicate pcr pid and bitrate to the smoother in the rx.recv() loop for setup
        let (pcr_tx, pcr_rx) = mpsc::channel();

        // setup callback for stream model to detect pcr pid
        let sm_callback = move |pat: &mut libltntstools_sys::pat_s| {
            log::info!("StreamModelCallback: received PAT.");

            if pat.program_count > 0 {
                log::info!(
                    "StreamModelCallback: PAT has {} programs.",
                    pat.program_count
                );
                for i in 0..pat.program_count {
                    let p = &pat.programs[i as usize];
                    log::info!(
                        "StreamModelCallback: Program #{}: PID: 0x{:x}",
                        i,
                        p.program_number
                    );
                    if p.pmt.PCR_PID > 0 {
                        log::info!(
                            "StreamModelCallback: Program #{}: PCR PID: 0x{:x}",
                            i,
                            p.pmt.PCR_PID
                        );
                        if let Err(e) = pcr_tx.send(p.pmt.PCR_PID) {
                            log::error!(
                                "StreamModelCallback: Failed to send PCR to smoother: {}",
                                e
                            );
                        }
                        break; // Only one program handled for now
                    }
                }
            } else {
                log::error!("StreamModelCallback: PAT has no programs.");
                return;
            }
        };

        let mut model = StreamModel::new(sm_callback);

        let mut smoother: Option<PcrSmoother<Box<dyn Fn(Vec<u8>) + Send>>> = None;

        while let Ok(seg) = rx.recv() {
            log::debug!(
                "SenderThread: processing segment #{} of {}bytes and {}s duration",
                seg.id,
                seg.data.len(),
                seg.duration
            );

            // write the segment to the model for the pcr pid detection if not given
            if pcr_pid <= 0 {
                let _ = model.write(&seg.data);
                if let Ok(pcr_pid_detected) = pcr_rx.try_recv() {
                    if pcr_pid != pcr_pid_detected as u16 {
                        log::info!("SenderThread: received new PCR PID: {}", pcr_pid_detected);
                        pcr_pid = pcr_pid_detected as u16;
                    }
                }
            }

            // Send to the smoother if we have a pcr pid
            if pcr_pid > 0 {
                // setup smoother if not already setup
                if smoother.is_none() {
                    smoother = Some(PcrSmoother::new(
                        pcr_pid,
                        rate,
                        pkt_size,
                        latency,
                        Box::new(smoother_callback),
                    ));
                }
                if let Some(ref mut s) = smoother {
                    let _ = s.write(&seg.data);
                }
            }
        }
        drop(smoother);
        drop(model);
        println!("SenderThread: exiting.");
    })
}

fn get_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn main() -> Result<()> {
    let matches = ClapCommand::new("hls-udp-streamer")
        .version(get_version())
        .about("HLS to UDP relay using LibLTNTSTools StreamModel and Smoother")
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
                .default_value("1800")
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .default_value("0")
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("latency")
                .short('l')
                .long("latency")
                .default_value("100")
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("pcr_pid")
                .short('c')
                .long("pcr-pid")
                .default_value("0x00")
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("rate")
                .short('r')
                .long("rate")
                .default_value("5000")
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("packet_size")
                .short('k')
                .long("packet-size")
                .default_value("1316")
                .action(ArgAction::Set),
        )
        .get_matches();

    let pkt_size = matches
        .get_one::<String>("packet_size")
        .unwrap()
        .parse::<i32>()
        .unwrap_or(1316);
    let rate = matches
        .get_one::<String>("rate")
        .unwrap()
        .parse::<i32>()
        .unwrap_or(5000);
    let latency = matches
        .get_one::<String>("latency")
        .unwrap()
        .parse::<i32>()
        .unwrap_or(100);
    let pcr_pid_str = matches.get_one::<String>("pcr_pid").unwrap();
    let pcr_pid = if pcr_pid_str.starts_with("0x") {
        u16::from_str_radix(&pcr_pid_str[2..], 16).unwrap_or(0x00)
    } else {
        pcr_pid_str.parse::<u16>().unwrap_or(0x00)
    };
    let m3u8_url = matches.get_one::<String>("m3u8_url").unwrap().clone();
    let udp_out = matches.get_one::<String>("udp_output").unwrap().clone();
    let poll_ms = matches
        .get_one::<String>("poll_ms")
        .unwrap()
        .parse::<u64>()
        .unwrap_or(100);
    let hist_cap = matches
        .get_one::<String>("history_size")
        .unwrap()
        .parse::<usize>()
        .unwrap_or(1800);
    let verbose = matches
        .get_one::<String>("verbose")
        .unwrap()
        .parse::<u8>()
        .unwrap_or(0);
    if verbose > 0 {
        let log_level = match verbose {
            1 => log::LevelFilter::Info,
            2 => log::LevelFilter::Debug,
            _ => log::LevelFilter::Trace,
        };
        env_logger::Builder::new()
            .filter_level(log_level)
            .format_timestamp_secs()
            .init();
    } else {
        env_logger::Builder::new()
            .filter_level(log::LevelFilter::Info)
            .format_timestamp_secs()
            .init();
    }
    log::info!("HLStoUDP: Logging initialized. Starting main()...");

    println!("Starting streamer:");
    println!("  M3U8 URL: {}", m3u8_url);
    println!("  UDP Target: {}", udp_out);
    println!("  Poll Interval: {} ms", poll_ms);
    println!("  History Size: {}", hist_cap);
    println!("  Latency: {} ms", latency);
    println!("  PCR PID: 0x{:x}", pcr_pid);
    println!("  Rate: {}", rate);
    println!("  Packet Size: {} bytes", pkt_size);

    let (tx, rx) = mpsc::channel();
    let dl_handle = receiver_thread(m3u8_url, poll_ms, hist_cap, tx);
    let sender_handle = sender_thread(udp_out, latency, pcr_pid, pkt_size, rate, rx);

    dl_handle.join().ok();
    sender_handle.join().ok();

    println!("Main thread exiting.");
    Ok(())
}
