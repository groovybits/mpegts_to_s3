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

// --------------------- MpegTsTableCache ---------------------

#[derive(Default)]
struct MpegTsTableCache {
    latest_pat: Option<[u8; 188]>,
    latest_pmt: Option<[u8; 188]>,
    pmt_pid: Option<u16>,
}

impl MpegTsTableCache {
    /// Store the entire 188-byte PAT packet, parse out the first PMT PID.
    /// Real production code should handle multiple TS packets for the PAT section,
    /// CRC checks, multi-program scenarios, etc.
    fn update_pat(&mut self, pkt: &[u8]) {
        // Must be exactly one TS packet of 188 bytes
        if pkt.len() != 188 {
            return;
        }

        // Keep a copy of the raw packet
        let mut arr = [0u8; 188];
        arr.copy_from_slice(pkt);
        self.latest_pat = Some(arr);

        // --- 1) Parse TS header (first 4 bytes) ---
        let transport_error_indicator = (pkt[1] & 0x80) != 0;
        if transport_error_indicator {
            // If error bit is set, ignore
            return;
        }
        let adaptation_control = (pkt[3] >> 4) & 0x3;
        // b11 means “adaptation + payload”, b01 means “payload only”

        let mut payload_offset = 4; // after TS header

        // --- 2) If there's an adaptation field, skip it ---
        if adaptation_control == 0b10 || adaptation_control == 0b11 {
            let adaptation_length = pkt[payload_offset] as usize;
            payload_offset += 1; // skip length byte
            if payload_offset + adaptation_length > 188 {
                // corrupt
                return;
            }
            payload_offset += adaptation_length;
        }

        // If there's no payload, we can't parse further
        if payload_offset >= 188 {
            return;
        }

        // --- 3) Pointer field (1 byte) ---
        let pointer_field = pkt[payload_offset] as usize;
        payload_offset += 1;
        if payload_offset + pointer_field >= 188 {
            // corrupt or incomplete
            return;
        }
        payload_offset += pointer_field;

        if payload_offset + 8 > 188 {
            // at least 8 bytes needed for a minimal section header
            return;
        }

        // --- 4) Now we should be at the start of the PAT section ---
        let table_id = pkt[payload_offset];
        if table_id != 0x00 {
            // 0x00 = PAT
            return;
        }

        let section_length =
            u16::from_be_bytes([pkt[payload_offset + 1] & 0x0F, pkt[payload_offset + 2]]);

        // safety check
        if (section_length as usize) > 1021 {
            return;
        }

        // Program loop
        let _transport_stream_id =
            u16::from_be_bytes([pkt[payload_offset + 3], pkt[payload_offset + 4]]);
        let _version_number = (pkt[payload_offset + 5] & 0x3E) >> 1; // prefix with underscore to avoid warnings
        let mut program_info_offset = payload_offset + 8;

        let pat_section_end = payload_offset + 3 + (section_length as usize);
        while program_info_offset + 4 <= pat_section_end.saturating_sub(4) {
            let program_number =
                u16::from_be_bytes([pkt[program_info_offset], pkt[program_info_offset + 1]]);
            let pid_hi = pkt[program_info_offset + 2] & 0x1F;
            let pid_lo = pkt[program_info_offset + 3];
            let pmt_pid = ((pid_hi as u16) << 8) | (pid_lo as u16);

            if program_number != 0 {
                self.pmt_pid = Some(pmt_pid);
                break;
            }
            program_info_offset += 4;
        }
    }

    /// Store the entire 188-byte PMT packet in `latest_pmt`.
    fn update_pmt(&mut self, pkt: &[u8]) {
        if pkt.len() == 188 {
            let mut arr = [0u8; 188];
            arr.copy_from_slice(pkt);
            self.latest_pmt = Some(arr);
        }
    }

    /// Write PAT + PMT if we have them
    fn write_tables<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        if let Some(ref pat) = self.latest_pat {
            writer.write_all(pat)?;
        }
        if let Some(ref pmt) = self.latest_pmt {
            writer.write_all(pmt)?;
        }
        Ok(())
    }
}

// ------------- MANUAL SEGMENTER -------------

/// Simple time-based segmenter for MPEG-TS packets.
/// Rotates segments every `SEGMENT_DURATION_SECONDS`.
const SEGMENT_DURATION_SECONDS: u64 = 10;

struct ManualSegmenter {
    output_dir: String,
    current_ts_file: Option<BufWriter<fs::File>>,
    current_segment_start: Instant,
    segment_index: u64,
    playlist_path: PathBuf,
    m3u8_initialized: bool,

    // optional reference to table cache (only used if user wants to inject PAT/PMT)
    pat_pmt_cache: Option<Arc<Mutex<MpegTsTableCache>>>,
    inject_pat_pmt: bool,
}

impl ManualSegmenter {
    fn new(output_dir: &str) -> Self {
        let playlist_path = Path::new("").join("index.m3u8");
        Self {
            output_dir: output_dir.to_string(),
            current_ts_file: None,
            current_segment_start: Instant::now(),
            segment_index: 0,
            playlist_path,
            m3u8_initialized: false,

            pat_pmt_cache: None,
            inject_pat_pmt: false,
        }
    }

    /// For convenience, set the optional table cache + boolean
    fn with_pat_pmt(
        mut self,
        table_cache: Option<Arc<Mutex<MpegTsTableCache>>>,
        inject: bool,
    ) -> Self {
        self.pat_pmt_cache = table_cache;
        self.inject_pat_pmt = inject;
        self
    }

    /// Write TS data to the “current segment,” rotating if necessary.
    fn write_ts(&mut self, data: &[u8]) -> std::io::Result<()> {
        if self.current_ts_file.is_none() {
            self.open_new_segment_file()?;
        }

        let elapsed = self.current_segment_start.elapsed().as_secs_f64();
        if elapsed >= SEGMENT_DURATION_SECONDS as f64 {
            self.close_current_segment_file()?;
            self.open_new_segment_file()?;
        }

        if let Some(file) = self.current_ts_file.as_mut() {
            file.write_all(data)?;
        }
        Ok(())
    }

    /// Close the current .ts file, finalize it, and update the .m3u8
    fn close_current_segment_file(&mut self) -> std::io::Result<()> {
        if self.current_ts_file.is_some() {
            let duration = SEGMENT_DURATION_SECONDS as f64;
            let segment_path = self.current_segment_path(self.segment_index + 1);
            self.current_ts_file.take(); // drop the writer

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

        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = fs::File::create(&full_path)?;
        let mut writer = BufWriter::new(file);

        // If the user wants to inject PAT/PMT, do so now
        if self.inject_pat_pmt {
            if let Some(cache) = &self.pat_pmt_cache {
                let cache = cache.lock().unwrap();
                cache.write_tables(&mut writer)?;
            }
        }

        self.current_ts_file = Some(writer);

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

        writeln!(f, "#EXTM3U")?;
        writeln!(f, "#EXT-X-VERSION:3")?;
        writeln!(f, "#EXT-X-TARGETDURATION:{}", SEGMENT_DURATION_SECONDS + 1)?;
        writeln!(f, "#EXT-X-MEDIA-SEQUENCE:0")?;
        Ok(())
    }

    /// Append an #EXTINF line + segment URI to the .m3u8
    fn append_m3u8_entry(&self, segment_path: &Path, duration: f64) -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(&self.playlist_path)?;
        writeln!(f, "#EXTINF:{:.6},", duration)?;
        writeln!(f, "{}/{}", self.output_dir, segment_path.to_string_lossy())?;
        Ok(())
    }

    fn current_segment_path(&self, index: u64) -> PathBuf {
        use chrono::Local;
        let now = Local::now();
        let year = now.format("%Y").to_string();
        let month = now.format("%m").to_string();
        let day = now.format("%d").to_string();
        let hour = now.format("%H").to_string();
        let timestamp = now.format("%Y%m%d-%H%M%S").to_string();
        let filename = format!("segment_{}__{:04}.ts", timestamp, index);
        Path::new(&year)
            .join(&month)
            .join(&day)
            .join(&hour)
            .join(filename)
    }
}

impl Drop for ManualSegmenter {
    fn drop(&mut self) {
        let _ = self.close_current_segment_file();
    }
}

// ------------- MAIN -------------
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
        .arg(
            Arg::new("inject_pat_pmt")
                .long("inject_pat_pmt")
                .help("If using manual segmentation, prepend the latest PAT & PMT to each segment.")
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
    let inject_pat_pmt = matches.get_flag("inject_pat_pmt");

    info!(
         "MpegTS to S3: endpoint={}, region={}, bucket={}, udp_ip={}, udp_port={}, \
          interface={}, timeout={}, output_dir={}, remove_local={}, manual_segment={}, inject_pat_pmt={}",
         endpoint,
         region_name,
         bucket,
         filter_ip,
         filter_port,
         interface,
         timeout,
         output_dir,
         remove_local,
         manual_segment,
         inject_pat_pmt
     );

    fs::create_dir_all(output_dir)?;

    info!("Initializing S3 client with endpoint: {}", endpoint);
    let creds = Credentials::new("minioadmin", "minioadmin", None, None, "dummy");

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

    info!("Starting directory watcher on: {}", output_dir);
    let (watch_tx, watch_rx) = channel();
    let watch_dir = output_dir.to_string();
    let watch_thread = thread::spawn(move || {
        if let Err(e) = watch_directory(&watch_dir, watch_tx) {
            eprintln!("Directory watcher error: {:?}", e);
        }
    });

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
        (None, None)
    };

    // Create MpegTsTableCache if we might use it
    let table_cache = Arc::new(Mutex::new(MpegTsTableCache::default()));

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

    let mut cap = Capture::from_device(interface.as_str())?
        .promisc(true)
        .buffer_size(8 * 1024 * 1024)
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

    let mut manual_segmenter = if manual_segment {
        let seg = ManualSegmenter::new(output_dir)
            .with_pat_pmt(Some(table_cache.clone()), inject_pat_pmt);
        Some(seg)
    } else {
        None
    };

    loop {
        if let Some(child) = ffmpeg_child.as_mut() {
            if let Some(exit_status) = child.try_wait()? {
                eprintln!("FFmpeg ended. Code: {:?}", exit_status.code());
                break;
            }
        }

        let packet = match cap.next_packet() {
            Ok(pkt) => pkt,
            Err(_) => {
                if let Some(child) = ffmpeg_child.as_mut() {
                    if let Some(exit_status) = child.try_wait()? {
                        eprintln!("FFmpeg ended. Code: {:?}", exit_status.code());
                        break;
                    }
                }
                continue;
            }
        };

        if let Some(ts_payload) = extract_mpegts_payload(&packet.data, filter_ip, filter_port) {
            if manual_segment && inject_pat_pmt {
                let pkt_count = ts_payload.len() / 188;
                let mut cache = table_cache.lock().unwrap();
                for i in 0..pkt_count {
                    let pkt = &ts_payload[i * 188..(i + 1) * 188];
                    let pid = ((pkt[1] as u16 & 0x1F) << 8) | (pkt[2] as u16);
                    if pid == 0 {
                        cache.update_pat(pkt);
                    } else if let Some(pp) = cache.pmt_pid {
                        if pid == pp {
                            cache.update_pmt(pkt);
                        }
                    }
                }
            }

            if let Some(buf) = ffmpeg_stdin_buf.as_mut() {
                if let Err(e) = buf.write_all(ts_payload) {
                    eprintln!("Error feeding data to FFmpeg: {:?}", e);
                    break;
                }
            }
            if let Some(seg) = manual_segmenter.as_mut() {
                if let Err(e) = seg.write_ts(ts_payload) {
                    eprintln!("Error writing manual TS segment: {:?}", e);
                    break;
                }
            }
        }
    }

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

    drop(manual_segmenter);

    let _ = upload_task.await?;
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

                            let should_upload = {
                                let tracker = tracker_clone.lock().unwrap();
                                !tracker.is_uploaded(&path_str) && !tracker.is_too_old(&path)
                            };

                            if should_upload {
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
        return None;
    }

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
        return None;
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

    if ts_payload.len() < 188 {
        warn!("Short TS packet found of size {}", ts_payload.len());
        return None;
    }

    let num_packets = ts_payload.len() / 188;
    let aligned_len = num_packets * 188;

    for i in 0..num_packets {
        if ts_payload[i * 188] != 0x47 {
            warn!("Misaligned TS packet found at offset {}", i * 188);
            return None;
        }
    }
    Some(&ts_payload[..aligned_len])
}
