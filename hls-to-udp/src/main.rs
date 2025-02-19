use anyhow::{anyhow, Result};
use clap::{Arg, ArgAction, Command as ClapCommand};
use ctrlc;
use env_logger;
use mpegts_pid_tracker::{PidTracker, TS_PACKET_SIZE};
#[cfg(feature = "smoother")]
use libltntstools::{PcrSmoother, StreamModel};
use m3u8_rs::{parse_playlist_res, Playlist};
use reqwest::blocking::Client;
use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::{self, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use url::Url;

#[derive(Debug)]
struct DownloadedSegment {
    id: usize,
    data: Vec<u8>,
    duration: f64,
    uri: String,
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
        .map_err(|e| anyhow!("HLStoUDP: ResolveSegmentUrl: Failed URL join: {}", e))
}

fn receiver_thread(
    m3u8_url: String,
    start_time: u64,
    end_time: u64,
    poll_interval_ms: u64,
    hist_capacity: usize,
    vod: bool,
    tx: SyncSender<DownloadedSegment>,
    shutdown_flag: Arc<AtomicBool>,
    tx_shutdown: SyncSender<()>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let client = Client::new();
        let base_url = match Url::parse(&m3u8_url) {
            Ok(u) => u,
            Err(e) => {
                log::error!("HLStoUDP: ReceiverThread Invalid M3U8 URL: {}", e);
                return;
            }
        };
        let mut seg_history = SegmentHistory::new(hist_capacity);
        let mut next_seg_id: usize = 1;
        let mut first_poll = if vod { false } else { true };

        // Main loop
        loop {
            if shutdown_flag.load(Ordering::SeqCst) {
                println!("HLStoUDP: ReceiverThread Shutdown flag set, exiting.");
                break;
            }
            let playlist_text = match client.get(&m3u8_url).send() {
                Ok(r) => {
                    if !r.status().is_success() {
                        log::error!(
                            "HLStoUDP: ReceiverThread 3U8 fetch HTTP error: {}",
                            r.status()
                        );
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    match r.text() {
                        Ok(txt) => txt,
                        Err(e) => {
                            log::error!("HLStoUDP: ReceiverThread M3U8 read error: {}", e);
                            thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                    }
                }
                Err(e) => {
                    log::error!("HLStoUDP: ReceiverThread M3U8 request error: {}", e);
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };

            let parsed = parse_playlist_res(playlist_text.as_bytes());
            let media_pl = match parsed {
                Ok(Playlist::MediaPlaylist(mp)) => mp,
                Ok(Playlist::MasterPlaylist(mp)) => {
                    log::info!("HLStoUDP: ReceiverThread Got Master playlist, parsing...");

                    /* get the variant that has the largest bandwidth */
                    let mut variant = None;
                    let mut best_bandwidth = 0;
                    for v in &mp.variants {
                        if v.bandwidth > best_bandwidth {
                            best_bandwidth = v.bandwidth;
                            variant = Some(v);
                        }
                    }
                    if variant.is_none() {
                        log::error!("HLStoUDP: ReceiverThread No valid variant found.");
                        thread::sleep(Duration::from_millis(poll_interval_ms));
                        continue;
                    }
                    let best_playlist = if let Some(variant) = variant {
                        let mut variant_uri = variant.uri.clone();
                        /* check if we have a relative url or not via http: prefix, adjust variant_uri if needed, getting the original m3u8_url's base */
                        if variant_uri.starts_with("http:")
                            || variant_uri.starts_with("https:")
                            || variant_uri.starts_with(".")
                            || variant_uri.starts_with("/")
                            || variant_uri.starts_with("#")
                        {
                            if variant_uri.starts_with("#") {
                                continue;
                            }
                        } else {
                            let mut base_uri = base_url.clone();
                            /* get path dirname without end file */
                            let path_parts: Vec<&str> = base_uri.path_segments().unwrap().collect();
                            let path_dirname = path_parts.join("/");
                            base_uri.set_path(path_dirname.as_str());
                            base_uri.set_query(None);
                            base_uri.set_fragment(None);
                            variant_uri = base_uri.join(&variant_uri).unwrap().to_string();
                        };
                        let variant_playlist_text = match client.get(&variant_uri).send() {
                            Ok(r) => {
                                if !r.status().is_success() {
                                    log::error!(
                                        "HLStoUDP: ReceiverThread Variant playlist {} fetch HTTP error: {}",
                                        variant_uri,
                                        r.status()
                                    );
                                    continue;
                                }
                                match r.text() {
                                    Ok(txt) => txt,
                                    Err(e) => {
                                        log::error!(
                                            "HLStoUDP: ReceiverThread Variant playlist {} read error: {}",
                                            variant_uri,
                                            e
                                        );
                                        continue;
                                    }
                                }
                            }
                            Err(e) => {
                                log::error!(
                                    "HLStoUDP: ReceiverThread Variant playlist {} request error: {}",
                                    variant_uri,
                                    e
                                );
                                continue;
                            }
                        };
                        let pl = parse_playlist_res(variant_playlist_text.as_bytes());
                        let variant_media_pl = match pl {
                            Ok(Playlist::MediaPlaylist(mp)) => mp,
                            Ok(Playlist::MasterPlaylist(_)) => {
                                log::error!("HLStoUDP: ReceiverThread Got nested Master playlist, ignoring...");
                                continue;
                            }
                            Err(e) => {
                                log::error!(
                                    "HLStoUDP: ReceiverThread Parse error in variant playlist: {:?}, ignoring...",
                                    e
                                );
                                continue;
                            }
                        };
                        variant_media_pl
                    } else {
                        log::error!("HLStoUDP: ReceiverThread No valid sub-playlist found.");
                        thread::sleep(Duration::from_millis(poll_interval_ms));
                        continue;
                    };
                    best_playlist
                }
                Err(e) => {
                    log::error!("HLStoUDP: ReceiverThread Parse error: {:?}, ignoring...", e);
                    thread::sleep(Duration::from_millis(poll_interval_ms));
                    continue;
                }
            };

            if media_pl.segments.is_empty() {
                log::warn!("HLStoUDP: ReceiverThread No segments found in playlist.");
                thread::sleep(Duration::from_millis(poll_interval_ms));
                continue;
            }

            if first_poll {
                for seg in &media_pl.segments {
                    seg_history.insert(seg.uri.to_string());
                }
                first_poll = false;
                thread::sleep(Duration::from_millis(poll_interval_ms));
                continue;
            }

            for seg in &media_pl.segments {
                if shutdown_flag.load(Ordering::SeqCst) {
                    println!("HLStoUDP: ReceiverThread Shutdown flag set, breaking loop.");
                    break;
                }

                // Format of segments for time calculations
                // http://127.0.0.1:9000/hls/channel01/2025/02/05/06/segment_20250205-060936__8511.ts
                // bucket, channel, year, month, day, hour, segment_YYYYMMDD-HHMMSS__INDEX.ts

                let mut seg_uri = seg.uri.clone();
                if seg_uri.starts_with("http:")
                    || seg_uri.starts_with("https:")
                    || seg_uri.starts_with(".")
                    || seg_uri.starts_with("/")
                    || seg_uri.starts_with("#")
                {
                    if seg_uri.starts_with("#") {
                        continue;
                    }
                } else {
                    let mut base_uri = base_url.clone();
                    /* get path dirname without end file */
                    let path_parts: Vec<&str> = base_uri.path_segments().unwrap().collect();
                    let path_dirname = path_parts.join("/");
                    base_uri.set_path(path_dirname.as_str());
                    base_uri.set_query(None);
                    base_uri.set_fragment(None);
                    seg_uri = base_uri.join(&seg_uri).unwrap().to_string();
                };
                let uri = &seg_uri;

                /* confirm a valid uri */
                if uri.is_empty() {
                    log::error!("HLStoUDP: ReceiverThread Empty segment URI, skipping.");
                    continue;
                }

                if seg_history.contains(uri) {
                    continue;
                }

                /* check uri is proper format with uri crate */
                if Url::parse(uri).is_err() {
                    log::error!("HLStoUDP: ReceiverThread Bad format for URI: {}", uri);
                    continue;
                }

                /* make sure we have a .ts extension on the uri, split off the ?... parts first */
                let uri_main = uri.split('?').next().unwrap();
                if !uri_main.ends_with(".ts") {
                    log::error!(
                        "HLStoUDP: ReceiverThread Bad segment, not a .ts file URI: {}",
                        uri
                    );
                    continue;
                }

                seg_history.insert(uri.to_string());

                // If VOD mode is enabled, filter segments based on the start_time and end_time
                if vod && (start_time > 0 || end_time > 0) {
                    let parse_segment_offset = |uri: &str| -> Option<u64> {
                        let filename = uri.split('/').last()?;
                        if !filename.starts_with("segment_") {
                            return None;
                        }
                        let content = &filename["segment_".len()..]; // "YYYYMMDD-HHMMSS__INDEX.ts"
                        let parts: Vec<&str> = content.split("__").collect();
                        let datetime_part = parts.get(0)?; // "YYYYMMDD-HHMMSS"
                        let dt_parts: Vec<&str> = datetime_part.split('-').collect();
                        if dt_parts.len() != 2 {
                            return None;
                        }
                        let time_str = dt_parts[1]; // "HHMMSS"
                        if time_str.len() != 6 {
                            return None;
                        }
                        // Parse the minute and second fields only, ignoring the hour.
                        let min: u64 = time_str[2..4].parse().ok()?;
                        let sec: u64 = time_str[4..6].parse().ok()?;
                        // Return the offset in milliseconds from the beginning of the hour.
                        Some((min * 60 + sec) * 1000)
                    };

                    if let Some(offset_ms) = parse_segment_offset(uri) {
                        if offset_ms < start_time {
                            log::debug!(
                                "HLStoUDP: Skipping segment {}: offset {}ms is before start_time {}ms",
                                uri,
                                offset_ms,
                                start_time
                            );
                            continue;
                        }
                        if end_time > 0 && offset_ms > end_time {
                            log::debug!(
                                "HLStoUDP: Segment {} offset {}ms exceeds end_time {}ms",
                                uri,
                                offset_ms,
                                end_time
                            );
                            if vod {
                                log::debug!(
                                    "HLStoUDP: VOD mode: finished processing required time range."
                                );
                            }
                            break;
                        }
                    }
                }

                let seg_url = match resolve_segment_url(&base_url, uri) {
                    Ok(u) => u,
                    Err(e) => {
                        log::error!(
                            "HLStoUDP: ReceiverThread Bad segment URL: {} for url {}",
                            e,
                            uri
                        );
                        continue;
                    }
                };
                log::debug!(
                    "HLStoUDP: ReceiverThread Downloading segment {} => {}",
                    next_seg_id,
                    seg_url
                );

                let seg_bytes = match client.get(seg_url.clone()).send() {
                    Ok(resp) => {
                        if !resp.status().is_success() {
                            log::error!(
                                "HLStoUDP: ReceiverThread Segment fetch error: {} for url {}",
                                resp.status(),
                                seg_url
                            );
                            continue;
                        }
                        match resp.bytes() {
                            Ok(b) => b.to_vec(),
                            Err(e) => {
                                log::error!(
                                    "HLStoUDP: ReceiverThread Segment read err: {} for url {}",
                                    e,
                                    seg_url
                                );
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        log::error!(
                            "HLStoUDP: ReceiverThread Segment request error: {} for url {}",
                            e,
                            seg_url
                        );
                        continue;
                    }
                };

                let seg_struct = DownloadedSegment {
                    id: next_seg_id,
                    data: seg_bytes,
                    duration: seg.duration.into(),
                    uri: uri.to_string(),
                };
                if tx.send(seg_struct).is_err() {
                    // get error information
                    log::error!("HLStoUDP: ReceiverThread Receiver dropped, downloader exiting");
                    tx_shutdown.send(()).ok();
                    return;
                }
                /* sleep duration of segment */
                if vod {
                    thread::sleep(Duration::from_millis(((seg.duration * 1000.0) * 0.80) as u64));
                }

                next_seg_id += 1;
            }

            if media_pl.end_list {
                log::warn!("HLStoUDP: ReceiverThread ENDLIST found => done downloading.");
                break;
            } else if vod {
                log::warn!("HLStoUDP: ReceiverThread VOD mode: done downloading.");
                break;
            } else {
                thread::sleep(Duration::from_millis(poll_interval_ms));
            }
        }
    })
}

fn sender_thread(
    udp_addr: String,
    latency: i32,
    pcr_pid_arg: u16,
    pkt_size: i32,
    min_pkt_size: i32,
    smoother_buffers: i32,
    smoother_max_bytes: usize,
    udp_queue_size: usize,
    udp_send_buffer: usize,
    use_smoother: bool,
    vod: bool,
    output_file: String,
    rx: mpsc::Receiver<DownloadedSegment>,
    shutdown_flag: Arc<AtomicBool>,
    tx_shutdown: SyncSender<()>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        #[cfg(not(feature = "smoother"))]
        let pcr_pid = pcr_pid_arg;
        #[cfg(feature = "smoother")]
        let mut pcr_pid = pcr_pid_arg;
        let mut total_bytes_dropped = 0usize;
        let mut total_bytes_sent = 0usize;

        log::debug!(
            "SenderThread: Starting UDP sender thread with input values vod={} udp_addr={}, latency={}, pcr_pid={}, pkt_size={}, smoother_buffers={}, smoother_max_bytes={}, udp_queue_size={}, udp_send_buffer={}, use_smoother={}",
            vod, udp_addr, latency, pcr_pid, pkt_size, smoother_buffers, smoother_max_bytes, udp_queue_size, udp_send_buffer, use_smoother
        );

        // Create raw socket with socket2
        let socket = match socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        ) {
            Ok(s) => s,
            Err(e) => {
                log::error!("HLStoUDP: SenderThread Socket creation error: {}", e);
                return;
            }
        };

        // Configure socket before converting to std::net::UdpSocket
        if udp_send_buffer > 0 {
            log::info!(
                "HLStoUDP: SenderThread Setting send buffer to {} bytes.",
                udp_send_buffer
            );
            let desired_send_buffer = udp_send_buffer;
            if let Err(e) = socket.set_send_buffer_size(desired_send_buffer) {
                log::error!("HLStoUDP: SenderThread Failed to set send buffer: {}", e);
            }
            match socket.send_buffer_size() {
                Ok(actual) => {
                    if actual < desired_send_buffer {
                        log::warn!(
                            "HLStoUDP: OS allocated {}KB send buffer (requested {}KB). Consider increasing system limits.",
                            actual / 1024,
                            desired_send_buffer / 1024
                        );
                    }
                }
                Err(e) => log::error!("HLStoUDP: SenderThread Couldn't get buffer size: {}", e),
            }
        }

        // Convert to stdlib socket
        let sock: UdpSocket = socket.into();

        // Keep socket in non-blocking mode (hybrid approach: we do small manual blocking when full)
        if let Err(e) = sock.set_nonblocking(true) {
            log::error!(
                "HLStoUDP: SenderThread Failed to set non-blocking socket: {}",
                e
            );
            return;
        }

        // Connect the UDP socket
        if let Err(e) = sock.connect(&udp_addr) {
            log::error!(
                "HLStoUDP: SenderThread: Failed to connect UDP socket: {}",
                e
            );
            return;
        }

        let sock = Arc::new(sock); // Wrap in Arc for thread safety

        // --- Channels for the Smoother callback => UDP send thread
        let (udp_tx, udp_rx) = mpsc::sync_channel::<Arc<Vec<u8>>>(udp_queue_size);

        // To avoid moved value errors, clone the channel senders for use in the smoother callback.
        #[cfg(feature = "smoother")]
        let udp_callback_arc = {
            let udp_tx_cloned = udp_tx.clone();
            let tx_shutdown_cloned = tx_shutdown.clone();
            // Wrap the callback in an Arc so we can clone it later when creating the smoother.
            Arc::new(move |v: Vec<u8>| {
                log::debug!(
                    "HLStoUDP: SmootherCallback received buffer with {} bytes for UDP.",
                    v.len()
                );
                // Wrap the Vec<u8> in an Arc before sending.
                if let Err(e) = udp_tx_cloned.send(Arc::new(v)) {
                    log::error!(
                        "HLStoUDP SmootherCallback: Failed to send buffer to UDP thread: {}",
                        e
                    );
                    // Attempt to shut down gracefully if the channel is disconnected
                    tx_shutdown_cloned.send(()).ok();
                }
            })
        };

        // --- UDP-sending thread, reading from udp_rx channel
        let sock_clone = Arc::clone(&sock);
        let shutdown_flag_clone = Arc::clone(&shutdown_flag);

        // Time-based approach to blocking, define a max wait:
        let max_block_ms = 10000; // ms total wait if OS buffer is full
        let timeout_interval = Duration::from_millis(3000); // 3 seconds

        // Use a VecDeque as a ring buffer to avoid memmove overhead.
        // We'll buffer the 7 * 188 byte packets till we have a complete packet and up to N MB.
        let mut buffer: VecDeque<u8> = VecDeque::with_capacity(1024 * 1024 * 10);
        let min_packet_size = min_pkt_size as usize;
        let max_packet_size = pkt_size as usize;

        let mut frame_time_micros = 10; // N micros per 188 byte packet
        let wait_time_micros = 1000; // 1ms wait time when blocking

        let capture_start_time = Instant::now();

        let udp_sender_thread_shutdown_flag = Arc::clone(&shutdown_flag);

        let udp_sender_thread = thread::spawn(move || {
            println!("HLStoUDP: UDPThread started (hybrid non-blocking + partial block).");

            // Create a PidTracker to track PIDs and Continuity Counter Errors
            let mut pid_tracker = PidTracker::new();

            while !udp_sender_thread_shutdown_flag.load(Ordering::SeqCst) {
                // We poll for new chunks with a short timeout, so we can check shutdown.
                match udp_rx.recv_timeout(timeout_interval) {
                    Ok(arc_data) => {
                        // Use the inner Vec<u8> from the Arc wrapper.
                        let data = arc_data.as_ref();
                        if shutdown_flag_clone.load(Ordering::SeqCst) {
                            log::warn!("HLStoUDP: UDPThread Shutdown flag set, exiting.");
                            break;
                        }

                        // Check if appending data would exceed our buffer capacity.
                        if buffer.len() + data.len() > buffer.capacity() {
                            log::warn!(
                                "HLStoUDP: UDPThread Buffer full, dropping data. (buffer={} bytes, data={} bytes)",
                                buffer.len(),
                                data.len()
                            );
                            continue;
                        }

                        // Extend our VecDeque with the new data.
                        buffer.extend(data.iter().copied());

                        // Process only if we have enough data for a full packet
                        if buffer.len() < min_packet_size {
                            log::debug!(
                                "HLStoUDP: UDPThread Buffer not full yet ({} bytes).",
                                buffer.len()
                            );
                            continue;
                        }

                        // Attempt to send this data to UDP
                        let buffer_start_time = Instant::now();
                        let mut index = 0;
                        let mut last_packet_send_time = Instant::now();

                        while buffer.len() >= min_packet_size {
                            // Determine the packet size to send (up to max_packet_size, aligned on TS_PACKET_SIZE)
                            let mut packet_size = max_packet_size;
                            if buffer.len() < max_packet_size {
                                packet_size = buffer.len();
                                // Align with TS_PACKET_SIZE boundary.
                                packet_size = packet_size - (packet_size % TS_PACKET_SIZE);
                            }
                            // Drain exactly packet_size bytes from the front of the VecDeque.
                            let mut chunk: Vec<u8> = buffer.drain(0..packet_size).collect();
                            let mut chunk_dropped = false;

                            // Check each TS packet for continuity errors
                            for packet_chunk in chunk.chunks(TS_PACKET_SIZE) {
                                if let Err(e) = pid_tracker
                                    .process_packet("UDPsender".to_string(), packet_chunk)
                                {
                                    if e == 0xFFFF {
                                        log::error!(
                                            "HLStoUDP: (UDPSender) Bad packet of size {} bytes for {} of {} chunks of {} bytes total is bad, dropping segment.",
                                            packet_chunk.len(),
                                            index,
                                            chunk.len() / TS_PACKET_SIZE,
                                            chunk.len()
                                        );
                                        index += 1;
                                        continue;
                                    } else {
                                        log::error!(
                                        "HLStoUDP: (UDPsender) Error processing TS packet: {:?}",
                                        e
                                    );
                                    }
                                }
                            }

                            loop {
                                // Calculate sending rate information.
                                let elapsed_ms = capture_start_time.elapsed().as_millis();
                                let sent_bytes = total_bytes_sent;
                                let sent_bps = if elapsed_ms <= 0 {
                                    0
                                } else {
                                    sent_bytes as u32 * 8 / elapsed_ms as u32
                                };
                                log::debug!(
                                    "HLStoUDP: UDPThread Sending {} bytes buffer at {} bps (sent={} bytes, dropped={} bytes)",
                                    chunk.len(),
                                    sent_bps,
                                    total_bytes_sent,
                                    total_bytes_dropped
                                );

                                /* get current bitrate and adjust frame_time_micros to keep bitrate from bursting */
                                let elapsed = last_packet_send_time.elapsed();
                                let elapsed_micros = elapsed.as_micros();
                                if elapsed_micros > 0 {
                                    let sent_bps =
                                        (chunk.len() as u64 * 8 * 1000000) / elapsed_micros as u64;
                                    log::debug!(
                                        "HLStoUDP: UDPThread Sent {} bytes in {} micros, rate {} bps.",
                                        chunk.len(),
                                        elapsed_micros,
                                        sent_bps
                                    );
                                    // Adjust frame_time_micros to keep bitrate from bursting
                                    if sent_bps > 0 {
                                        frame_time_micros =
                                            (chunk.len() as u64 * 8 * 1000000) / sent_bps;
                                    }
                                }

                                // calculate how long we should sleep to maintain a constant rate
                                let sleep_time_micros: u64 = (chunk.len() / TS_PACKET_SIZE) as u64
                                    * frame_time_micros as u64;

                                match sock_clone.send(&chunk) {
                                    Ok(bytes_sent) => {
                                        log::debug!(
                                            "HLStoUDP: UDPThread Packet of {} bytes sent from a chunk of {} bytes at {} bps.",
                                            bytes_sent,
                                            chunk.len(),
                                            sent_bps
                                        );
                                        if bytes_sent < chunk.len() {
                                            log::warn!(
                                                "HLStoUDP: UDPThread Partial send of {} bytes from a chunk of {} bytes at {} bps.",
                                                bytes_sent,
                                                chunk.len(),
                                                sent_bps
                                            );
                                            // Remove the sent bytes and retry sending the remainder.
                                            chunk = chunk[bytes_sent..].to_vec();
                                            continue;
                                        }
                                        total_bytes_sent += chunk.len();
                                        /* check last send time and calculate packet time left to wait */
                                        let elapsed = last_packet_send_time.elapsed();
                                        if ! vod && elapsed < Duration::from_micros(sleep_time_micros) {
                                            let sleep_time =
                                                Duration::from_micros(sleep_time_micros) - elapsed;
                                            thread::sleep(sleep_time);
                                        }
                                        last_packet_send_time = Instant::now();
                                        break;
                                    }
                                    Err(e) => {
                                        if e.kind() == std::io::ErrorKind::WouldBlock {
                                            let elapsed_ms =
                                                buffer_start_time.elapsed().as_millis();
                                            if elapsed_ms > max_block_ms as u128 {
                                                total_bytes_dropped += chunk.len();
                                                chunk_dropped = true;
                                                log::error!(
                                                    "HLStoUDP: UDPThread Socket still full after {} ms. Dropping {} byte chunk. (sent={} bytes, dropped={} bytes, rate={} bps)",
                                                    elapsed_ms,
                                                    chunk.len(),
                                                    total_bytes_sent,
                                                    total_bytes_dropped,
                                                    sent_bps
                                                );
                                                break;
                                            }
                                            log::warn!(
                                                "HLStoUDP: UDPThread Socket full, waiting for buffer space for {} bytes, elapsed {} ms, rate {} bps.",
                                                chunk.len(), elapsed_ms, sent_bps
                                            );
                                            thread::sleep(Duration::from_micros(wait_time_micros));
                                        } else {
                                            total_bytes_dropped += chunk.len();
                                            chunk_dropped = true;
                                            log::error!(
                                                "HLStoUDP: UDPThread UDP error: {}. Dropping {} byte chunk. (sent={} bytes, dropped={} bytes, rate={} bps)",
                                                e,
                                                chunk.len(),
                                                total_bytes_sent,
                                                total_bytes_dropped,
                                                sent_bps
                                            );
                                            break;
                                        }
                                    }
                                }
                            }
                            if chunk_dropped {
                                log::debug!(
                                    "HLStoUDP: UDPThread Chunk of size {} bytes dropped due to blocking or error.",
                                    chunk.len()
                                );
                            }
                            index += 1;
                        }

                        // Finally, check for shutdown again
                        if shutdown_flag_clone.load(Ordering::SeqCst) {
                            log::warn!("HLStoUDP: UDPThread Shutdown flag set, exiting.");
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        // Periodically check shutdown
                        if shutdown_flag_clone.load(Ordering::SeqCst) {
                            log::warn!("HLStoUDP: UDPThread Shutdown flag set, exiting.");
                            break;
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        log::warn!("HLStoUDP: UDPThread channel disconnected, exiting.");
                        break;
                    }
                }
            }
            log::warn!(
                "HLStoUDP: UDPThread exiting. Sent={} bytes, Dropped={} bytes",
                total_bytes_sent,
                total_bytes_dropped
            );
        });

        // --- If we are auto-detecting PCR, create a channel from the StreamModel callback
        #[cfg(feature = "smoother")]
        let (pcr_tx, pcr_rx) = mpsc::channel();

        #[cfg(feature = "smoother")]
        let sm_callback = move |pat: &mut libltntstools_sys::pat_s| {
            log::warn!("HLStoUDP: StreamModelCallback received PAT.");
            if pat.program_count > 0 {
                log::debug!(
                    "HLStoUDP: StreamModelCallback PAT has {} programs.",
                    pat.program_count
                );
                for i in 0..pat.program_count {
                    let p = &pat.programs[i as usize];
                    log::debug!(
                        "HLStoUDP: StreamModelCallback Program #{}: PID: 0x{:x}",
                        i,
                        p.program_number
                    );
                    if p.pmt.PCR_PID > 0 {
                        log::info!(
                            "HLStoUDP: StreamModelCallback Program #{}: PCR PID: 0x{:x}",
                            i,
                            p.pmt.PCR_PID
                        );
                        let _ = pcr_tx.send(p.pmt.PCR_PID);
                        break;
                    }
                }
            } else {
                log::error!("StreamModelCallback: PAT has no programs.");
            }
        };

        // Conditionally create a StreamModel if pcr_pid_arg == 0
        #[cfg(feature = "smoother")]
        let mut model: Option<StreamModel<_>> = if pcr_pid_arg == 0 && use_smoother {
            Some(StreamModel::new(sm_callback))
        } else {
            None
        };

        // We'll create the smoother once we have a known pcr_pid
        #[cfg(feature = "smoother")]
        let mut smoother: Option<PcrSmoother<Box<dyn Fn(Vec<u8>) + Send>>> = None;

        // vecdec buffer to hold UDP packets while we are detecting the stream model PCR PID
        let mut buffer: VecDeque<u8> = VecDeque::with_capacity(1024 * 1024 * 10);

        // Process each downloaded segment from the receiver thread
        while let Ok(seg) = rx.recv() {
            log::debug!(
                "HLStoUDP: SenderThread Segment #{} => {} bytes, {} sec, URI: {}",
                seg.id,
                seg.data.len(),
                seg.duration,
                seg.uri
            );

            if shutdown_flag.load(Ordering::SeqCst) {
                log::warn!("HLStoUDP: SenderThread Shutdown flag set, exiting.");
                break;
            }

            // If we haven't locked a PCR PID yet, feed data to the StreamModel for detection
            #[cfg(feature = "smoother")]
            if pcr_pid == 0 && use_smoother {
                if let Some(ref mut m) = model {
                    // Write the seg.data to the model without copying: note that
                    // seg.data is later moved into our buffer.
                    let _ = m.write(&seg.data);
                    // Check whether a new PID was detected
                    if let Ok(detected) = pcr_rx.try_recv() {
                        pcr_pid = detected as u16;
                        log::warn!(
                            "HLStoUDP: SenderThread Detected new PCR PID = 0x{:x}",
                            pcr_pid
                        );
                    }
                }
            }

            // If we have a valid PCR PID, drop the model
            #[cfg(feature = "smoother")]
            if use_smoother && pcr_pid > 0 && model.is_some() {
                log::warn!("HLStoUDP: SenderThread Dropping model after PCR PID detected.");
                model.take(); // drop it
            }

            // Write to output file if requested
            if !output_file.is_empty() {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&output_file)
                {
                    if let Err(e) = f.write_all(&seg.data) {
                        log::error!(
                            "HLStoUDP: SenderThread Failed to write to output file: {}",
                            e
                        );
                    }
                } else {
                    log::error!(
                        "HLStoUDP: SenderThread Failed to open output file: {}",
                        output_file
                    );
                }
            }

            // Move the received data into our buffer without copying each byte.
            // Extend the VecDeque with the data from seg.data.
            if buffer.len() + seg.data.len() > buffer.capacity() {
                log::warn!(
                    "HLStoUDP: SenderThread Buffer full, dropping data. (buffer={} bytes, data={} bytes)",
                    buffer.len(),
                    seg.data.len()
                );
                continue;
            }
            buffer.extend(seg.data);

            // Process full TS packets from the buffer, removing data as it is sent.
            while (pcr_pid > 0 || !use_smoother) && buffer.len() >= min_packet_size {
                // Drain exactly TS_PACKET_SIZE bytes from the front.
                let packet_chunk: Vec<u8> = buffer.drain(0..min_packet_size).collect();

                if !use_smoother {
                    // Send the packet directly to the UDP sender thread wrapped in an Arc.
                    if let Err(e) = udp_tx.send(Arc::new(packet_chunk)) {
                        log::error!("HLStoUDP: SenderThread UDP send error: {}", e);
                    }
                } else {
                    // If we have a valid PCR PID, create the PcrSmoother
                    #[cfg(feature = "smoother")]
                    if smoother.is_none() {
                        // create the smoother using our cloned callback:
                        let smoother_callback = {
                            let cb = udp_callback_arc.clone();
                            move |v: Vec<u8>| {
                                (cb)(v);
                            }
                        };
                        smoother = Some(PcrSmoother::new(
                            pcr_pid,
                            smoother_buffers,
                            max_packet_size as i32,
                            latency,
                            Box::new(smoother_callback),
                        ));
                    }
                    #[cfg(feature = "smoother")]
                    if let Some(ref mut s) = smoother {
                        if let Err(e) = s.write(&packet_chunk) {
                            log::error!("HLStoUDP: SenderThread Smoother write error: {}", e);
                        }
                    }
                }
            }

            // If we have not yet determined a valid PCR PID,
            // keep the data in the buffer for later processing.
        }

        // Cleanup
        if use_smoother {
            #[cfg(feature = "smoother")]
            if let Some(m) = model.take() {
                log::warn!("HLStoUDP: SenderThread Dropping unused StreamModel on exit.");
                drop(m);
            }
            #[cfg(feature = "smoother")]
            if let Some(s) = smoother.take() {
                log::warn!("HLStoUDP: SenderThread Dropping smoother on exit.");
                drop(s);
            }
        }

        // Tell the udp_sender_thread to shut down
        log::warn!("HLStoUDP: SenderThread sending shutdown signal to smoother thread.");
        let _ = tx_shutdown.send(());

        // Wait for the smoother thread to exit
        log::info!("HLStoUDP: SenderThread waiting for smoother thread to exit...");
        let _ = udp_sender_thread.join();

        println!("HLStoUDP: SenderThread exiting.");
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
            eprintln!("HLStoUDP: Got CTRL+C, shutting down gracefully...");
            shutdown_flag.store(true, Ordering::SeqCst);
            ctrl_counter += 1;
            if ctrl_counter >= 3 {
                eprintln!("HLStoUDP: Got CTRL+C 3 times, forcing exit.");
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
                .default_value("999999")
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
                .default_value("2000")
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
            Arg::new("smoother_buffers")
                .long("smoother_buffers")
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
            Arg::new("min_packet_size")
                .short('m')
                .long("min-packet-size")
                .default_value("1316")
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("segment_queue_size")
                .short('q')
                .long("segment-queue-size")
                .default_value("10")
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("udp_queue_size")
                .short('z')
                .long("udp-queue-size")
                .default_value("512")
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("udp_send_buffer")
                .short('b')
                .long("udp-send-buffer")
                .default_value("0")
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("max_bytes_threshold")
                .long("max-bytes-threshold")
                .help("Maximum bytes in smoother queue before reset")
                .default_value("500_000_000"),
        )
        .arg(
            Arg::new("use_smoother")
                .long("use-smoother")
                .action(ArgAction::SetTrue)
                .help("Use the PcrSmoother for rate control (default: false)"),
        )
        .arg(
            Arg::new("vod")
                .long("vod")
                .action(ArgAction::SetTrue)
                .help("Use VOD mode (default: false)"),
        )
        .arg(
            Arg::new("start_time")
                .long("start-time")
                .help("Start time offset in milliseconds from start of m3u8 playlist for VOD mode (default: 0)")
                .default_value("0"),
        )
        .arg(
            Arg::new("end_time")
                .long("end-time")
                .help("End time offset in milliseconds from start of m3u8 playlist for VOD mode (default: 0 - end of playlist)")
                .default_value("0"),
        )
        .arg(
            Arg::new("output_file")
                .short('f')
                .long("output-file")
                .help("Output file for debugging")
                .default_value("")
                .action(ArgAction::Set),
        )
        .arg(
            Arg::new("quiet")
                .long("quiet")
                .help("Suppress all non error output")
                .action(ArgAction::SetTrue),
        )
        .get_matches();

    log::debug!("HLStoUDP: Command-line arguments parsed: {:?}", matches);
    println!("HLStoUDP: version: {}", get_version());

    let quiet = matches.get_flag("quiet");
    let output_file = matches.get_one::<String>("output_file").unwrap().clone();
    let start_time = matches
        .get_one::<String>("start_time")
        .unwrap()
        .parse::<u64>()
        .unwrap_or(0);
    let end_time = matches
        .get_one::<String>("end_time")
        .unwrap()
        .parse::<u64>()
        .unwrap_or(0);
    let vod = matches.get_flag("vod");
    #[cfg(not(feature = "smoother"))]
    let mut use_smoother = matches.get_flag("use_smoother");
    #[cfg(feature = "smoother")]
    let use_smoother = matches.get_flag("use_smoother");
    let max_bytes_threshold = matches
        .get_one::<String>("max_bytes_threshold")
        .unwrap()
        .parse::<usize>()
        .unwrap_or(500_000_000);
    let udp_queue_size = matches
        .get_one::<String>("udp_queue_size")
        .unwrap()
        .parse::<usize>()
        .unwrap_or(512);
    let udp_send_buffer = matches
        .get_one::<String>("udp_send_buffer")
        .unwrap()
        .parse::<usize>()
        .unwrap_or(0);
    let segment_queue_size = matches
        .get_one::<String>("segment_queue_size")
        .unwrap()
        .parse::<usize>()
        .unwrap_or(10);
    let pkt_size = matches
        .get_one::<String>("packet_size")
        .unwrap()
        .parse::<i32>()
        .unwrap_or(1316);
    let min_pkt_size = matches
        .get_one::<String>("min_packet_size")
        .unwrap()
        .parse::<i32>()
        .unwrap_or(1316);
    let smoother_buffers = matches
        .get_one::<String>("smoother_buffers")
        .unwrap()
        .parse::<i32>()
        .unwrap_or(5000);
    let latency = matches
        .get_one::<String>("latency")
        .unwrap()
        .parse::<i32>()
        .unwrap_or(2000);
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
        .unwrap_or(999999);
    let verbose = matches
        .get_one::<String>("verbose")
        .unwrap()
        .parse::<u8>()
        .unwrap_or(0);
    if quiet {
        env_logger::Builder::new()
            .filter_level(log::LevelFilter::Warn)
            .format_timestamp_secs()
            .init();
    } else if verbose > 0 {
        let log_level = match verbose {
            1 => log::LevelFilter::Warn,
            2 => log::LevelFilter::Info,
            3 => log::LevelFilter::Debug,
            _ => log::LevelFilter::Trace,
        };
        env_logger::Builder::new()
            .filter_level(log_level)
            .format_timestamp_secs()
            .init();
    } else {
        env_logger::Builder::new()
            .filter_level(log::LevelFilter::Error)
            .format_timestamp_secs()
            .init();
    }
    println!("HLStoUDP: Logging initialized. Starting main()...");

    #[cfg(feature = "smoother")]
    {
        log::warn!("HLStoUDP: LibLTNTSTools enabled.");
    }
    #[cfg(not(feature = "smoother"))]
    {
        log::warn!("HLStoUDP: LibLTNTSTools disabled.");
        if use_smoother {
            log::warn!("HLStoUDP: Smoother requested but LibLTNTSTools is disabled.");
            log::warn!("HLStoUDP: Disabling smoother.");
            use_smoother = false;
        }
    }

    let (tx, rx) = mpsc::sync_channel(segment_queue_size);
    let (tx_shutdown, rx_shutdown) = mpsc::sync_channel(1000);
    let receiver_handle = receiver_thread(
        m3u8_url,
        start_time,
        end_time,
        poll_ms,
        hist_cap,
        vod.clone(),
        tx,
        shutdown_flag.clone(),
        tx_shutdown.clone(),
    );
    let sender_handle = sender_thread(
        udp_out,
        latency,
        pcr_pid,
        pkt_size,
        min_pkt_size,
        smoother_buffers,
        max_bytes_threshold,
        udp_queue_size,
        udp_send_buffer,
        use_smoother,
        vod,
        output_file,
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
            log::warn!("HLStoUDP: Shutdown signal received, breaking loop.");
            shutdown_flag.store(true, Ordering::SeqCst);
            break;
        }
    }

    log::info!("Main: Waiting for sender thread to exit...");
    sender_handle.join().unwrap();

    println!("Main: Waiting for receiver thread to exit...");
    receiver_handle.join().unwrap();

    println!("HLStoUDP: Main thread exiting.");
    Ok(())
}
