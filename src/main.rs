/*
 * mpegts_to_s3
 *
 * This program captures MPEG-TS packets from a UDP multicast stream, either:
 *   A) Feeds them into FFmpeg, which segments into HLS .ts/.m3u8, OR
 *   B) Manually segments the MPEG-TS and writes an .m3u8 playlist.
 *
 * Then, a directory watcher picks up new/modified files and uploads them to an S3 bucket.
 *
 * Chris Kennedy 2025 Jan 15
 */
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use aws_types::region::Region;
use clap::{Arg, Command as ClapCommand};
use log::{debug, info, warn};
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher,
};
use pcap::Capture;
use std::collections::HashSet;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use tokio::time::sleep;

// ------------- HELPER STRUCTS & FUNCS -------------

struct FileTracker {
    uploaded_files: HashSet<String>,
    max_age: Duration,
}

impl FileTracker {
    fn new(max_age_secs: u64) -> Self {
        FileTracker {
            uploaded_files: HashSet::new(),
            max_age: Duration::from_secs(max_age_secs),
        }
    }

    fn is_uploaded(&self, path: &str) -> bool {
        self.uploaded_files.contains(path)
    }

    fn mark_uploaded(&mut self, path: String) {
        self.uploaded_files.insert(path);
    }

    fn is_too_old(&self, path: &Path) -> bool {
        if let Ok(metadata) = path.metadata() {
            if let Ok(modified) = metadata.modified() {
                if let Ok(age) = SystemTime::now().duration_since(modified) {
                    return age > self.max_age;
                }
            }
        }
        false
    }
}

async fn ensure_bucket_exists(
    client: &Client,
    bucket: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match client.head_bucket().bucket(bucket).send().await {
        Ok(_) => {
            info!("Bucket {} exists", bucket);
            Ok(())
        }
        Err(_) => {
            info!("Creating bucket {}", bucket);
            client.create_bucket().bucket(bucket).send().await?;
            info!("Successfully created bucket {}", bucket);
            Ok(())
        }
    }
}

// ------------- MANUAL SEGMENTER -------------

/// Simple time-based segmenter for MPEG-TS packets.
/// Rotates segments every `SEGMENT_DURATION_SECONDS`.
const SEGMENT_DURATION_SECONDS: u64 = 2;

struct ManualSegmenter {
    output_dir: String,
    current_ts_file: Option<BufWriter<fs::File>>,
    current_segment_start: Instant,
    segment_index: u64,
    playlist_path: PathBuf,
    m3u8_initialized: bool,
}

impl ManualSegmenter {
    fn new(output_dir: &str) -> Self {
        let playlist_path = Path::new(output_dir).join("index.m3u8");
        Self {
            output_dir: output_dir.to_string(),
            current_ts_file: None,
            current_segment_start: Instant::now(),
            segment_index: 0,
            playlist_path,
            m3u8_initialized: false,
        }
    }

    /// Write TS data to the “current segment,” rotating if necessary.
    fn write_ts(&mut self, data: &[u8]) -> std::io::Result<()> {
        if self.current_ts_file.is_none() {
            self.open_new_segment_file()?;
        }

        // If we've been writing to the current segment for longer than SEGMENT_DURATION_SECONDS,
        // rotate to a new file. (We do this purely by wall-clock time.)
        let elapsed = self.current_segment_start.elapsed().as_secs_f64();
        if elapsed >= SEGMENT_DURATION_SECONDS as f64 {
            self.close_current_segment_file()?;
            self.open_new_segment_file()?;
        }

        // Append data
        if let Some(file) = self.current_ts_file.as_mut() {
            file.write_all(data)?;
        }
        Ok(())
    }

    /// Close the current .ts file, finalize it, and update the .m3u8
    fn close_current_segment_file(&mut self) -> std::io::Result<()> {
        if self.current_ts_file.is_some() {
            // We consider the entire time “SEGMENT_DURATION_SECONDS”
            let duration = SEGMENT_DURATION_SECONDS as f64;
            let segment_path = self.current_segment_path(self.segment_index);
            self.current_ts_file.take(); // drop the writer

            // Update the playlist with an #EXTINF entry for the just-finished segment
            self.append_m3u8_entry(&segment_path, duration)?;
            self.segment_index += 1;
        }
        Ok(())
    }

    /// Open a new .ts file for writing + update tracking fields
    fn open_new_segment_file(&mut self) -> std::io::Result<()> {
        self.current_segment_start = Instant::now();
        let segment_path = self.current_segment_path(self.segment_index);
        let full_path = Path::new(&self.output_dir).join(&segment_path);

        // Ensure subdirectories exist
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = fs::File::create(&full_path)?;
        self.current_ts_file = Some(BufWriter::new(file));

        // If m3u8 isn't inited, write boilerplate
        if !self.m3u8_initialized {
            self.init_m3u8()?;
            self.m3u8_initialized = true;
        }
        Ok(())
    }

    /// Write the standard HLS preamble if not yet written
    fn init_m3u8(&self) -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.playlist_path)?;

        // Minimal playlist example:
        // #EXTM3U
        // #EXT-X-VERSION:3
        // #EXT-X-TARGETDURATION:3
        // #EXT-X-MEDIA-SEQUENCE:0
        writeln!(f, "#EXTM3U")?;
        writeln!(f, "#EXT-X-VERSION:3")?;
        writeln!(f, "#EXT-X-TARGETDURATION:{}", SEGMENT_DURATION_SECONDS + 1)?;
        writeln!(f, "#EXT-X-MEDIA-SEQUENCE:0")?;
        Ok(())
    }

    /// Append an #EXTINF line + segment URI to the .m3u8
    fn append_m3u8_entry(&self, segment_path: &Path, duration: f64) -> std::io::Result<()> {
        // Just open in append mode
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(&self.playlist_path)?;
        writeln!(f, "#EXTINF:{:.6},", duration)?;
        // On the next line, put the relative path
        writeln!(f, "{}", segment_path.to_string_lossy())?;
        Ok(())
    }

    /// Compute the subpath for the Nth segment with date expansions (year/month/day/hour).
    fn current_segment_path(&self, index: u64) -> PathBuf {
        use chrono::Local;
        let now = Local::now();
        let year = now.format("%Y").to_string();
        let month = now.format("%m").to_string();
        let day = now.format("%d").to_string();
        let hour = now.format("%H").to_string();
        let timestamp = now.format("%Y%m%d-%H%M%S").to_string();
        // same naming style as FFmpeg:
        //  e.g. "hls/2025/01/14/23/segment_20250114-234519_0000.ts"
        let filename = format!("segment_{}__{:04}.ts", timestamp, index);
        Path::new(&year)
            .join(&month)
            .join(&day)
            .join(&hour)
            .join(filename)
    }
}

impl Drop for ManualSegmenter {
    /// Ensure we cleanly close the last segment if still open.
    fn drop(&mut self) {
        let _ = self.close_current_segment_file();
    }
}

// ------------- MAIN -------------
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // --------------------------------------------------------------------
    // 1) Parse CLI
    // --------------------------------------------------------------------
    let matches = ClapCommand::new("mpegts_to_s3")
        .version("1.0")
        .about("PCAP capture -> HLS -> Directory Watch -> S3 Upload")
        .arg(
            Arg::new("endpoint")
                .short('e')
                .long("endpoint")
                .default_value("http://127.0.0.1:9000")
                .help("S3-compatible endpoint"),
        )
        .arg(
            Arg::new("region")
                .short('r')
                .long("region")
                .default_value("us-west-2")
                .help("S3 region"),
        )
        .arg(
            Arg::new("bucket")
                .short('b')
                .long("bucket")
                .default_value("ltnhls")
                .help("S3 bucket name"),
        )
        .arg(
            Arg::new("udp_ip")
                .short('i')
                .long("udp_ip")
                .default_value("227.1.1.102")
                .help("UDP multicast IP to filter"),
        )
        .arg(
            Arg::new("udp_port")
                .short('p')
                .long("udp_port")
                .default_value("4102")
                .help("UDP port to filter"),
        )
        .arg(
            Arg::new("interface")
                .short('n')
                .long("interface")
                .default_value("net1")
                .help("Network interface for pcap"),
        )
        .arg(
            Arg::new("timeout")
                .short('t')
                .long("timeout")
                .default_value("1000")
                .help("Capture timeout in milliseconds"),
        )
        .arg(
            Arg::new("output_dir")
                .short('o')
                .long("output_dir")
                .default_value("hls")
                .help("Local dir for HLS output (could be a RAM disk)"),
        )
        .arg(
            Arg::new("remove_local")
                .long("remove_local")
                .help("Remove local .ts/.m3u8 after uploading to S3?")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("manual_segment")
                .long("manual_segment")
                .help("Perform manual TS segmentation + .m3u8 generation (no FFmpeg).")
                .action(clap::ArgAction::SetTrue),
        )
        .get_matches();

    let endpoint = matches.get_one::<String>("endpoint").unwrap();
    let region_name = matches.get_one::<String>("region").unwrap();
    let bucket = matches.get_one::<String>("bucket").unwrap();
    let filter_ip = matches.get_one::<String>("udp_ip").unwrap();
    let filter_port: u16 = matches.get_one::<String>("udp_port").unwrap().parse()?;
    let interface = matches.get_one::<String>("interface").unwrap();
    let timeout: i32 = matches.get_one::<String>("timeout").unwrap().parse()?;
    let output_dir = matches.get_one::<String>("output_dir").unwrap();
    let remove_local = matches.get_flag("remove_local");
    let manual_segment = matches.get_flag("manual_segment");

    info!(
        "MpegTS to S3: endpoint={}, region={}, bucket={}, udp_ip={}, udp_port={}, \
          interface={}, timeout={}, output_dir={}, remove_local={}, manual_segment={}",
        endpoint,
        region_name,
        bucket,
        filter_ip,
        filter_port,
        interface,
        timeout,
        output_dir,
        remove_local,
        manual_segment
    );

    fs::create_dir_all(output_dir)?;

    // --------------------------------------------------------------------
    // 2) Initialize S3 Client
    // --------------------------------------------------------------------
    info!("Initializing S3 client with endpoint: {}", endpoint);
    let creds = Credentials::new(
        "minioadmin", // access key
        "minioadmin", // secret key
        None,         // session token
        None,         // expiry
        "dummy",      // provider name
    );

    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(Region::new(region_name.clone()))
        .endpoint_url(endpoint)
        .credentials_provider(creds)
        .load()
        .await;

    let s3_config = aws_sdk_s3::config::Builder::from(&config)
        .force_path_style(true)
        .build();
    let s3_client = Client::from_conf(s3_config);

    ensure_bucket_exists(&s3_client, bucket).await?;

    // --------------------------------------------------------------------
    // 3) Directory Watcher
    // --------------------------------------------------------------------
    info!("Starting directory watcher on: {}", output_dir);
    let (watch_tx, watch_rx) = channel();
    let watch_dir = output_dir.to_string();
    let watch_thread = thread::spawn(move || {
        if let Err(e) = watch_directory(&watch_dir, watch_tx) {
            eprintln!("Directory watcher error: {:?}", e);
        }
    });

    // --------------------------------------------------------------------
    // 4) Possibly spawn FFmpeg (if NOT manual_segment)
    // --------------------------------------------------------------------
    let (mut ffmpeg_child, mut ffmpeg_stdin_buf) = if !manual_segment {
        // FFmpeg-based HLS
        let hls_segment_filename =
            format!("{}/%Y/%m/%d/%H/segment_%Y%m%d-%H%M%S_%04d.ts", output_dir);
        let m3u8_output = "index.m3u8".to_string();

        info!("Starting FFmpeg with output: {}", m3u8_output);

        let mut child = Command::new("ffmpeg")
            .arg("-i")
            .arg("pipe:0")
            .arg("-c")
            .arg("copy")
            .arg("-loglevel")
            .arg("error")
            .arg("-y")
            .arg("-hide_banner")
            .arg("-nostats")
            .arg("-max_delay")
            .arg("500000")
            .arg("-f")
            .arg("hls")
            .arg("-map")
            .arg("0")
            .arg("-hls_time")
            .arg("2")
            .arg("-hls_segment_type")
            .arg("mpegts")
            .arg("-hls_playlist_type")
            .arg("event")
            .arg("-hls_list_size")
            .arg("0")
            .arg("-strftime")
            .arg("1")
            .arg("-strftime_mkdir")
            .arg("1")
            .arg("-hls_segment_filename")
            .arg(&hls_segment_filename)
            .arg(&m3u8_output)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;

        let child_stdin = child.stdin.take().ok_or("Failed to take FFmpeg stdin")?;
        let buf = BufWriter::new(child_stdin);

        (Some(child), Some(buf))
    } else {
        // Manual segmentation
        (None, None)
    };

    // --------------------------------------------------------------------
    // 5) Launch an async task to watch new/modified files -> upload S3
    // --------------------------------------------------------------------
    let s3_client_clone = s3_client.clone();
    let bucket_clone = bucket.clone();
    let output_dir_clone = output_dir.clone();
    let upload_task = tokio::spawn(async move {
        handle_file_events(
            watch_rx,
            s3_client_clone,
            bucket_clone,
            output_dir_clone,
            remove_local,
        )
        .await;
    });

    // --------------------------------------------------------------------
    // 6) Start capturing with pcap
    // --------------------------------------------------------------------
    let mut cap = Capture::from_device(interface.as_str())?
        .promisc(true)
        .buffer_size(8 * 1024 * 1024) // 8MB buffer
        .snaplen(65535)
        .timeout(100)
        .timeout(timeout)
        .open()?;

    let filter_expr = format!("udp and host {} and port {}", filter_ip, filter_port);
    cap.filter(&filter_expr, true)?;

    println!(
        "Capturing on '{}', listening for {}:{}, writing HLS to '{}'",
        interface, filter_ip, filter_port, output_dir
    );

    // If in manual mode, create the segmenter
    let mut manual_segmenter = if manual_segment {
        Some(ManualSegmenter::new(output_dir))
    } else {
        None
    };

    loop {
        // (a) If FFmpeg is active, check if it exited
        if let Some(child) = ffmpeg_child.as_mut() {
            if let Some(exit_status) = child.try_wait()? {
                eprintln!("FFmpeg ended. Code: {:?}", exit_status.code());
                break;
            }
        }

        // (b) Grab next pcap packet
        let packet = match cap.next_packet() {
            Ok(pkt) => pkt,
            Err(_) => {
                // Possibly a timeout
                if let Some(child) = ffmpeg_child.as_mut() {
                    if let Some(exit_status) = child.try_wait()? {
                        eprintln!("FFmpeg ended. Code: {:?}", exit_status.code());
                        break;
                    }
                }
                continue;
            }
        };

        // (c) Extract the TS payload
        if let Some(ts_payload) = extract_mpegts_payload(&packet.data, filter_ip, filter_port) {
            // (d) If NOT manual_segment, feed data to FFmpeg
            if let Some(buf) = ffmpeg_stdin_buf.as_mut() {
                if let Err(e) = buf.write_all(ts_payload) {
                    eprintln!("Error feeding data to FFmpeg: {:?}", e);
                    break;
                }
            }
            // (e) If manual_segment, feed data to ManualSegmenter
            if let Some(seg) = manual_segmenter.as_mut() {
                if let Err(e) = seg.write_ts(ts_payload) {
                    eprintln!("Error writing manual TS segment: {:?}", e);
                    break;
                }
            }
        }
    }

    // --------------------------------------------------------------------
    // 7) Done capturing. Clean up
    // --------------------------------------------------------------------

    // If we had FFmpeg, flush & close ffmpeg stdin
    if let Some(mut buf) = ffmpeg_stdin_buf.take() {
        let _ = buf.flush();
        drop(buf);

        if let Some(child) = ffmpeg_child.as_mut() {
            let ffmpeg_status = child.wait()?;
            if !ffmpeg_status.success() {
                eprintln!("FFmpeg exited with error code: {:?}", ffmpeg_status.code());
            } else {
                println!("FFmpeg finished successfully.");
            }
        }
    }

    // If manual segmenter was used, let it drop (closing last file)
    drop(manual_segmenter);

    // Wait for background upload thread
    let _ = upload_task.await?;
    // Wait for watcher thread
    let _ = watch_thread.join();

    println!("Exiting normally.");
    Ok(())
}

// ---------------- Directory Watcher ----------------

fn watch_directory(dir_path: &str, tx: std::sync::mpsc::Sender<Event>) -> NotifyResult<()> {
    let (notify_tx, notify_rx) = std::sync::mpsc::channel();
    let mut watcher: RecommendedWatcher = Watcher::new(
        notify_tx,
        notify::Config::default().with_compare_contents(false),
    )?;
    watcher.watch(Path::new(dir_path), RecursiveMode::Recursive)?;

    loop {
        match notify_rx.recv() {
            Ok(Ok(event)) => {
                if tx.send(event).is_err() {
                    break;
                }
            }
            Ok(Err(e)) => eprintln!("Notify error: {:?}", e),
            Err(_) => break,
        }
    }
    Ok(())
}

// ---------------- File-Event -> S3-Upload ----------------

async fn handle_file_events(
    rx: Receiver<Event>,
    s3_client: Client,
    bucket: String,
    base_dir: String,
    remove_local: bool,
) {
    let tracker = Arc::new(Mutex::new(FileTracker::new(3600))); // 1 hour max age

    while let Ok(event) = rx.recv() {
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                for path in event.paths {
                    if let Some(ext) = path.extension() {
                        if ext == "ts" || ext == "m3u8" {
                            let path_str = path.to_string_lossy().to_string();
                            let tracker_clone = Arc::clone(&tracker);

                            // Check if file was already uploaded or is too old
                            let should_upload = {
                                let tracker = tracker_clone.lock().unwrap();
                                !tracker.is_uploaded(&path_str) && !tracker.is_too_old(&path)
                            };

                            if should_upload {
                                // Wait a bit so writer is done
                                sleep(Duration::from_millis(300)).await;
                                if let Ok(()) = upload_file_to_s3(
                                    &s3_client,
                                    &bucket,
                                    &base_dir,
                                    &path,
                                    remove_local,
                                )
                                .await
                                {
                                    let mut tracker = tracker_clone.lock().unwrap();
                                    tracker.mark_uploaded(path_str);
                                }
                            } else {
                                debug!("Skipping file {}: already uploaded or too old", path_str);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

async fn upload_file_to_s3(
    s3_client: &Client,
    bucket: &str,
    base_dir: &str,
    path: &Path,
    remove_local: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let relative_path = strip_base_dir(path, base_dir)?;
    let key_str = relative_path.to_string_lossy().to_string();

    println!(
        "Uploading {} -> s3://{}/{}",
        path.display(),
        bucket,
        key_str
    );

    // Add retry logic
    let mut retries = 3;
    while retries > 0 {
        match s3_client
            .put_object()
            .bucket(bucket)
            .key(&key_str)
            .body(ByteStream::from_path(path).await?)
            .send()
            .await
        {
            Ok(_) => {
                println!("Uploaded {}", key_str);
                if remove_local {
                    if let Err(e) = fs::remove_file(path) {
                        eprintln!("Failed removing local file {}: {:?}", path.display(), e);
                    }
                }
                return Ok(());
            }
            Err(e) => {
                retries -= 1;
                if retries == 0 {
                    return Err(Box::new(e));
                }
                eprintln!(
                    "Upload failed, retrying ({} attempts left): {:?}",
                    retries, e
                );
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
    Ok(())
}

/// Strips the `base_dir` prefix from `full_path`.
/// e.g. if `full_path = hls/2025/01/14/23/segment_20250114-234519_0000.ts`
/// and `base_dir = "hls"`, returns `2025/01/14/23/segment_20250114-234519_0000.ts`.
fn strip_base_dir<'a>(
    full_path: &'a Path,
    base_dir: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let base = Path::new(base_dir).canonicalize()?;
    let full = full_path.canonicalize()?;
    let relative = full.strip_prefix(base)?;
    Ok(relative.to_path_buf())
}

// ---------------- PCAP -> TS Payload Parser ----------------

fn extract_mpegts_payload<'a>(
    data: &'a [u8],
    filter_ip: &str,
    filter_port: u16,
) -> Option<&'a [u8]> {
    if data.len() < 14 {
        return None;
    }
    let ethertype = u16::from_be_bytes([data[12], data[13]]);
    if ethertype != 0x0800 {
        return None; // not IPv4
    }

    // IP
    let ip_version_ihl = data[14];
    let ip_version = ip_version_ihl >> 4;
    if ip_version != 4 {
        return None;
    }
    let ip_header_len = (ip_version_ihl & 0x0f) * 4;
    let udp_header_offset = 14 + ip_header_len as usize;
    if data.len() < udp_header_offset + 8 {
        return None;
    }

    let protocol = data[23];
    if protocol != 17 {
        return None; // not UDP
    }

    let ip_dst_addr = &data[30..34];
    let dst_addr_str = format!(
        "{}.{}.{}.{}",
        ip_dst_addr[0], ip_dst_addr[1], ip_dst_addr[2], ip_dst_addr[3]
    );

    let udp_dst_port =
        u16::from_be_bytes([data[udp_header_offset + 2], data[udp_header_offset + 3]]);
    let udp_length =
        u16::from_be_bytes([data[udp_header_offset + 4], data[udp_header_offset + 5]]) as usize;

    if dst_addr_str != filter_ip || udp_dst_port != filter_port {
        return None;
    }

    let udp_payload_offset = udp_header_offset + 8;
    if data.len() < udp_payload_offset {
        return None;
    }
    let udp_payload_len = udp_length.saturating_sub(8);
    if data.len() < udp_payload_offset + udp_payload_len {
        return None;
    }

    let payload = &data[udp_payload_offset..udp_payload_offset + udp_payload_len];
    if payload.is_empty() {
        return None;
    }

    // If it's RTP carrying TS, we typically see 0x80 at [0] and TS sync (0x47) at [12].
    // If it's raw TS, we expect 0x47 at [0].
    let (ts_payload, is_rtp) = if payload[0] == 0x80 && payload.len() > 12 && payload[12] == 0x47 {
        (&payload[12..], true)
    } else if payload[0] == 0x47 {
        (payload, false)
    } else {
        warn!(
            "Unknown payload type found at offset {} of type 0x{:02x}, size {}",
            udp_payload_offset,
            payload[0],
            payload.len()
        );
        return None;
    };

    if is_rtp {
        debug!("RTP packet found with TS payload size {}", ts_payload.len());
    }

    // Validate TS packet alignment (188-byte packets)
    if ts_payload.len() < 188 {
        warn!("Short TS packet found of size {}", ts_payload.len());
        return None;
    }

    // # of complete TS packets
    let num_packets = ts_payload.len() / 188;
    let aligned_len = num_packets * 188;

    // Verify sync bytes
    for i in 0..num_packets {
        if ts_payload[i * 188] != 0x47 {
            warn!("Misaligned TS packet found at offset {}", i * 188);
            return None; // misaligned
        }
    }
    Some(&ts_payload[..aligned_len])
}
