use anyhow::{anyhow, Result};
use clap::{Arg, ArgAction, Command as ClapCommand};
use ctrlc;
use env_logger;
use libltntstools::{PcrSmoother, StreamModel};
use m3u8_rs::{parse_playlist_res, Playlist};
use reqwest::blocking::Client;
use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use url::Url;

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
    tx: SyncSender<DownloadedSegment>,
    shutdown_flag: Arc<AtomicBool>,
    tx_shutdown: SyncSender<()>,
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
            if shutdown_flag.load(Ordering::SeqCst) {
                log::info!("ReceiverThread: Shutdown flag set, exiting.");
                break;
            }
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
                if shutdown_flag.load(Ordering::SeqCst) {
                    log::info!("ReceiverThread: Shutdown flag set, breaking loop.");
                    break;
                }
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
                    tx_shutdown.send(()).ok();
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
    udp_queue_size: usize,
    retries: usize,
    rx: Receiver<DownloadedSegment>,
    shutdown_flag: Arc<AtomicBool>,
    tx_shutdown: SyncSender<()>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut pcr_pid = pcr_pid_arg;
        let mut stats_dropped = 0;
        let mut stats_sent = 0;

        // Create raw socket with socket2
        let socket = match socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("SenderThread: Socket creation error: {}", e);
                return;
            }
        };

        // Configure socket before converting to std::net::UdpSocket
        let desired_send_buffer = 2 * 1024 * 1024; // 2MB
        if let Err(e) = socket.set_send_buffer_size(desired_send_buffer) {
            eprintln!("SenderThread: Failed to set send buffer: {}", e);
        }

        // Get actual buffer size
        match socket.send_buffer_size() {
            Ok(actual) => {
                if actual < desired_send_buffer {
                    log::warn!(
                        "OS allocated {}KB send buffer (requested {}KB). Consider increasing system limits.",
                        actual / 1024,
                        desired_send_buffer / 1024
                    );
                }
            }
            Err(e) => eprintln!("SenderThread: Couldn't get buffer size: {}", e),
        }

        // Convert to stdlib socket
        let sock: std::net::UdpSocket = socket.into();

        // Set non-blocking mode to prevent send from blocking
        if let Err(e) = sock.set_nonblocking(true) {
            eprintln!("SenderThread: Failed to set non-blocking socket: {}", e);
            return;
        }

        if let Err(e) = sock.connect(&udp_addr) {
            eprintln!("SenderThread: Failed to connect UDP socket: {}", e);
            return;
        }
        let sock = Arc::new(sock); // Wrap in Arc for thread safety

        // setup channels for smoother callback to use to send data to a udp sending thread
        let (smoother_tx, smoother_rx) = mpsc::sync_channel(udp_queue_size);
        let smoother_callback = |v: Vec<u8>| {
            log::debug!(
                "SmootherCallback: received buffer with {} bytes, sending to UDP.",
                v.len()
            );
            if let Err(e) = smoother_tx.send(v) {
                log::error!(
                    "SmootherCallback: Failed to send buffer to UDP thread: {}",
                    e
                );
                tx_shutdown.send(()).ok();
            }
        };

        // smoother thread to send data to UDP from the packets sent from the smoother callback smoother_tx
        let sock_clone = Arc::clone(&sock);
        let shutdown_flag_clone = Arc::clone(&shutdown_flag);
        let smoother_thread = thread::spawn(move || {
            let mut retry_count = 0;
            while let Ok(data) = smoother_rx.recv() {
                log::debug!(
                    "SenderThread: SmootherThread: received buffer with {} bytes, sending to UDP.",
                    data.len()
                );
                if shutdown_flag_clone.load(Ordering::SeqCst) {
                    log::info!("SmootherThread: Shutdown flag set, exiting.");
                    break;
                }
                loop {
                    if shutdown_flag_clone.load(Ordering::SeqCst) {
                        log::info!("SmootherThread: Shutdown flag set, breaking loop.");
                        break;
                    }
                    match sock_clone.send(&data) {
                        Ok(_) => {
                            stats_sent += 1;
                            if retry_count > 0 {
                                log::warn!(
                                    "SmootherThread: Retried {} times, sent packet of sent={} dropped={}",
                                    retry_count,
                                    stats_sent,
                                    stats_dropped
                                );
                            }
                            retry_count = 0;
                            break;
                        }
                        Err(e) => {
                            // Handle WouldBlock specifically
                            if e.kind() == std::io::ErrorKind::WouldBlock {
                                log::warn!(
                                "SmootherThread: UDP socket buffer full, dropping packet of sent={} dropped={}",
                                stats_sent, stats_dropped
                                );
                            } else {
                                log::error!("SmootherThread: Fatal UDP error: {}", e);
                                retry_count = 0;
                                stats_dropped += 1;
                                break;
                            }
                        }
                    }
                    if retry_count >= retries {
                        log::error!(
                            "SmootherThread: Retried {} times, dropping packet of sent={} dropped={}",
                            retries,
                            stats_sent,
                            stats_dropped
                        );
                        stats_dropped += 1;
                        retry_count = 0;
                        break;
                    } else {
                        retry_count += 1;
                        // sleep 1ms before retrying
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                // free data buffer
                drop(data);
                if shutdown_flag_clone.load(Ordering::SeqCst) {
                    log::info!("SmootherThread: Shutdown flag set, exiting.");
                    break;
                }
            }
            log::info!("SmootherThread: exiting.");
        });

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

        let mut model: Option<StreamModel<_>> = if pcr_pid_arg == 0 {
            Some(StreamModel::new(sm_callback))
        } else {
            None
        };
        let mut smoother: Option<PcrSmoother<Box<dyn Fn(Vec<u8>) + Send>>> = None;

        while let Ok(seg) = rx.recv() {
            log::debug!(
                "SenderThread: processing segment #{} of {}bytes and {}s duration",
                seg.id,
                seg.data.len(),
                seg.duration
            );
            if shutdown_flag.load(Ordering::SeqCst) {
                log::info!("SenderThread: Shutdown flag set, exiting.");
                break;
            }

            // write the segment to the model for the pcr pid detection if not given
            if pcr_pid <= 0 && model.is_some() {
                if let Some(ref mut m) = model {
                    let _ = m.write(&seg.data);
                    if let Ok(pcr_pid_detected) = pcr_rx.try_recv() {
                        if pcr_pid != pcr_pid_detected as u16 {
                            log::info!("SenderThread: received new PCR PID: {}", pcr_pid_detected);
                            pcr_pid = pcr_pid_detected as u16;
                        }
                    }
                }
            }

            // drop the model if we have a pcr pid
            if pcr_pid > 0 && model.is_some() {
                log::warn!("SenderThread: Dropping model after PCR PID detected.");
                if let Some(m) = model.take() {
                    drop(m);
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

            if shutdown_flag.load(Ordering::SeqCst) {
                log::info!("SenderThread: Shutdown flag set, exiting.");
                break;
            }
        }
        if model.is_some() {
            log::warn!("SenderThread: Model not freed, dropping.");
            if let Some(m) = model {
                drop(m);
            }
        }
        if let Some(s) = smoother {
            log::info!("SenderThread: Dropping smoother.");
            drop(s);
        }

        log::info!("SenderThread: sending shutdown signal to smoother.");
        tx_shutdown.send(()).ok();

        // wait for smoother thread to exit
        log::info!("SenderThread: waiting for smoother thread to exit.");
        smoother_thread.join().unwrap();
        println!("SenderThread: exiting.");
    })
}

fn get_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn main() -> Result<()> {
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let mut ctrl_counter = 0;
    ctrlc::set_handler({
        let shutdown_flag = Arc::clone(&shutdown_flag);
        move || {
            eprintln!("Got CTRL+C, shutting down gracefully...");
            shutdown_flag.store(true, Ordering::SeqCst);
            ctrl_counter += 1;
            if ctrl_counter >= 3 {
                eprintln!("Got CTRL+C 3 times, forcing exit.");
                std::process::exit(1);
            }
        }
    })
    .expect("Error setting Ctrl-C handler");
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
                .default_value("1000")
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
        .arg(
            Arg::new("segment_queue_size")
                .short('q')
                .long("segment-queue-size")
                .default_value("32")
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("udp_queue_size")
                .short('z')
                .long("udp-queue-size")
                .default_value("1024")
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("max_bitrate")
                .long("max-bitrate")
                .help("Maximum output bitrate in kbps")
                .default_value("5000"),
        )
        .arg(
            Arg::new("max_burst")
                .long("max-burst")
                .help("Maximum burst size in kilobytes")
                .default_value("1000"),
        )
        .arg(
            Arg::new("send_retries")
                .long("send_retries")
                .help("Number of retries before dropping packet")
                .default_value("0"),
        )
        .get_matches();

    let send_retries = matches
        .get_one::<String>("send_retries")
        .unwrap()
        .parse::<usize>()
        .unwrap_or(0);
    let udp_queue_size = matches
        .get_one::<String>("udp_queue_size")
        .unwrap()
        .parse::<usize>()
        .unwrap_or(1024);
    let segment_queue_size = matches
        .get_one::<String>("segment_queue_size")
        .unwrap()
        .parse::<usize>()
        .unwrap_or(32);
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
        .unwrap_or(1000);
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
    println!("  Version: {}", get_version());
    println!("  M3U8 URL: {}", m3u8_url);
    println!("  UDP Target: {}", udp_out);
    println!("  Poll Interval: {} ms", poll_ms);
    println!("  History Size: {}", hist_cap);
    println!("  Latency: {} ms", latency);
    println!("  PCR PID: 0x{:x}", pcr_pid);
    println!("  Rate: {}", rate);
    println!("  Packet Size: {} bytes", pkt_size);
    println!("  Verbose: {}", verbose);
    println!("  Segment Queue Size: {}", segment_queue_size);
    println!("  UDP Queue Size: {}", udp_queue_size);
    println!("  Send Retries: {}", send_retries);

    let (tx, rx) = mpsc::sync_channel(segment_queue_size);
    let (tx_shutdown, rx_shutdown) = mpsc::sync_channel(1000);
    let dl_handle = receiver_thread(
        m3u8_url,
        poll_ms,
        hist_cap,
        tx,
        shutdown_flag.clone(),
        tx_shutdown.clone(),
    );
    let _sender_handle = sender_thread(
        udp_out,
        latency,
        pcr_pid,
        pkt_size,
        rate,
        udp_queue_size,
        send_retries,
        rx,
        shutdown_flag.clone(),
        tx_shutdown.clone(),
    );

    loop {
        thread::sleep(Duration::from_millis(100));
        if shutdown_flag.load(Ordering::SeqCst) {
            break;
        }
        if let Ok(_) = rx_shutdown.try_recv() {
            log::info!("Main: Shutdown signal received, breaking loop.");
            shutdown_flag.store(true, Ordering::SeqCst);
            break;
        }
    }

    log::info!("Main: Waiting for downloader thread to exit...");
    dl_handle.join().unwrap();

    // FIXME: This is causing a deadlock, need to figure out why
    //log::info!("Main: Waiting for sender thread to exit...");
    //sender_handle.join().unwrap();

    println!("HLStoUDP: Main thread exiting.");
    Ok(())
}
