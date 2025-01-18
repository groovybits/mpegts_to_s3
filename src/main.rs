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
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use aws_types::region::Region;
use clap::{Arg, Command as ClapCommand};
use get_if_addrs::get_if_addrs;
use log::{debug, error, info, warn};
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher,
};
use pcap::Capture;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::{BufWriter, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Mutex;
use tokio::time::sleep;

use env_logger;

// ------------- HELPER STRUCTS & FUNCS -------------

fn get_segment_duration_seconds() -> u64 {
    std::env::var("SEGMENT_DURATION_SECONDS")
        .unwrap_or_else(|_| "2".to_string())
        .parse()
        .unwrap_or(2)
}

fn get_max_segment_size_bytes() -> usize {
    std::env::var("MAX_SEGMENT_SIZE_BYTES")
        .unwrap_or_else(|_| "100000000".to_string())
        .parse()
        .unwrap_or(100_000_000)
}

fn get_file_max_age_seconds() -> u64 {
    std::env::var("FILE_MAX_AGE_SECONDS")
        .unwrap_or_else(|_| "30".to_string())
        .parse()
        .unwrap_or(30)
}

fn get_url_signing_seconds() -> u64 {
    std::env::var("URL_SIGNING_SECONDS")
        .unwrap_or_else(|_| "604800".to_string())
        .parse()
        .unwrap_or(604800)
}

fn get_s3_username() -> String {
    std::env::var("MINIO_ROOT_USER").unwrap_or_else(|_| "minioadmin".to_string())
}

fn get_s3_password() -> String {
    std::env::var("MINIO_ROOT_PASSWORD").unwrap_or_else(|_| "ThisIsSecret12345.".to_string())
}

fn get_pcap_packet_count() -> usize {
    std::env::var("PCAP_PACKET_COUNT")
        .unwrap_or_else(|_| "7".to_string())
        .parse()
        .unwrap_or(7)
}

fn get_pcap_packet_size() -> usize {
    std::env::var("PCAP_PACKET_SIZE")
        .unwrap_or_else(|_| "188".to_string())
        .parse()
        .unwrap_or(188)
}

fn get_pcap_packet_header_size() -> usize {
    std::env::var("PCAP_PACKET_HEADER_SIZE")
        .unwrap_or_else(|_| "42".to_string())
        .parse()
        .unwrap_or(42)
}

fn get_snaplen() -> i32 {
    let pcap_packet_count = get_pcap_packet_count();
    let pcap_packet_size = get_pcap_packet_size();
    let pcap_packet_header_size = get_pcap_packet_header_size();
    ((pcap_packet_count * pcap_packet_size) + pcap_packet_header_size)
        .try_into()
        .unwrap()
}

fn get_buffer_size() -> i32 {
    std::env::var("BUFFER_SIZE")
        .unwrap_or_else(|_| "4194304".to_string())
        .parse()
        .unwrap_or(1024 * 1024 * 4)
}

fn get_use_estimated_duration() -> bool {
    std::env::var("USE_ESTIMATED_DURATION")
        .unwrap_or_else(|_| "true".to_string())
        .parse()
        .unwrap_or(true)
}

fn get_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Utility: Attempt to get the actual duration from file creation/modification times.
/// Fallback to environment-based default if we can't measure it.
fn get_actual_file_duration(path: &Path) -> f64 {
    if let Ok(metadata) = path.metadata() {
        if let Ok(created_time) = metadata.created() {
            if let Ok(modified_time) = metadata.modified() {
                if let Ok(elapsed) = modified_time.duration_since(created_time) {
                    return elapsed.as_secs_f64();
                }
            }
        }
    }
    get_segment_duration_seconds() as f64
}

#[derive(Debug, Clone)]
pub struct HourlyIndexEntry {
    pub sequence_id: u64,
    pub duration: f64,
    pub signed_url: String,
    pub custom_lines: Vec<String>,
}

pub struct HourlyIndexCreator {
    s3_client: aws_sdk_s3::Client,
    bucket: String,
    generate_unsigned_urls: bool,
    endpoint: String,

    hour_map: std::collections::HashMap<String, Vec<HourlyIndexEntry>>,
    global_sequence_id: u64,
}

impl HourlyIndexCreator {
    pub fn new(
        s3_client: aws_sdk_s3::Client,
        bucket: String,
        generate_unsigned_urls: bool,
        endpoint: String,
    ) -> Self {
        Self {
            s3_client,
            bucket,
            generate_unsigned_urls,
            endpoint,
            hour_map: std::collections::HashMap::new(),
            global_sequence_id: 0,
        }
    }

    pub async fn record_segment(
        &mut self,
        hour_dir: &str,
        segment_key: &str,
        duration: f64,
        custom_lines: Vec<String>,
        output_dir: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.global_sequence_id += 1;
        let signed_url = self.generate_url(segment_key).await?;

        let entry = HourlyIndexEntry {
            sequence_id: self.global_sequence_id,
            duration,
            signed_url,
            custom_lines,
        };
        let entries = self.hour_map.entry(hour_dir.to_string()).or_default();
        entries.push(entry);

        self.write_hourly_index(hour_dir, output_dir).await?;
        Ok(())
    }

    async fn write_hourly_index(
        &mut self,
        hour_dir: &str,
        output_dir: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let local_dir = std::path::Path::new(output_dir).join(hour_dir);
        let index_path = local_dir.join("hourly_index.m3u8");
        let temp_path = local_dir.join("hourly_index_temp.m3u8");

        std::fs::create_dir_all(&local_dir)?;

        let entries = self.hour_map.get(hour_dir);

        {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&temp_path)?;

            writeln!(file, "#EXTM3U")?;
            writeln!(file, "#EXT-X-VERSION:3")?;

            let target_duration = if let Some(vec) = entries {
                let max_seg = vec
                    .iter()
                    .map(|e| e.duration.ceil() as u64)
                    .max()
                    .unwrap_or(get_segment_duration_seconds());
                max_seg
            } else {
                get_segment_duration_seconds()
            };
            writeln!(file, "#EXT-X-TARGETDURATION:{}", target_duration)?;

            let media_seq = if let Some(vec) = entries {
                vec.iter().map(|e| e.sequence_id).min().unwrap_or(0)
            } else {
                0
            };
            writeln!(file, "#EXT-X-MEDIA-SEQUENCE:{}", media_seq)?;

            if let Some(vec) = entries {
                let mut sorted = vec.clone();
                sorted.sort_by_key(|e| e.sequence_id);
                for entry in sorted {
                    for line in &entry.custom_lines {
                        writeln!(file, "{}", line)?;
                    }
                    writeln!(file, "#EXTINF:{:.6},", entry.duration)?;
                    writeln!(file, "{}", entry.signed_url)?;
                }
            }
        }

        std::fs::rename(&temp_path, &index_path)?;

        self.upload_local_file_to_s3(
            &index_path,
            &format!("{}/hourly_index.m3u8", hour_dir),
            "application/vnd.apple.mpegurl",
        )
        .await?;

        let final_index_url = self
            .presign_get_url(&format!("{}/hourly_index.m3u8", hour_dir))
            .await?;
        self.rewrite_urls_log(hour_dir, &final_index_url)?;
        Ok(())
    }

    fn rewrite_urls_log(
        &mut self,
        hour_dir: &str,
        final_url: &str,
    ) -> std::io::Result<()> {
        let log_path = std::path::Path::new("").join("urls.log");
        let temp_path = std::path::Path::new("").join("urls_temp.log");

        let mut lines = vec![];
        if log_path.exists() {
            let old_data = std::fs::read_to_string(&log_path)?;
            for ln in old_data.lines() {
                if !ln.starts_with(&format!("Hour {} =>", hour_dir)) {
                    lines.push(ln.to_string());
                }
            }
        }
        lines.push(format!("Hour {} => {}", hour_dir, final_url));

        {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&temp_path)?;
            for ln in &lines {
                writeln!(f, "{}", ln)?;
            }
        }
        std::fs::rename(&temp_path, &log_path)?;
        Ok(())
    }

    async fn generate_url(
        &self,
        object_key: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        if self.generate_unsigned_urls {
            Ok(format!("{}/{}/{}", self.endpoint, self.bucket, object_key))
        } else {
            let config =
                PresigningConfig::expires_in(Duration::from_secs(get_url_signing_seconds()))?;
            let presigned_req = self
                .s3_client
                .get_object()
                .bucket(&self.bucket)
                .key(object_key)
                .presigned(config)
                .await?;
            Ok(presigned_req.uri().to_string())
        }
    }

    async fn presign_get_url(
        &self,
        object_key: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        self.generate_url(object_key).await
    }

    async fn upload_local_file_to_s3(
        &self,
        local_path: &std::path::Path,
        object_key: &str,
        content_type: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!(
            "Uploading hourly index -> s3://{}/{} (content_type={})",
            self.bucket, object_key, content_type
        );
        let body_bytes = ByteStream::from_path(local_path).await?;
        self.s3_client
            .put_object()
            .bucket(&self.bucket)
            .key(object_key)
            .body(body_bytes)
            .cache_control("no-cache, no-store, must-revalidate")
            .content_type(content_type)
            .send()
            .await?;
        Ok(())
    }
}

fn find_ipv4_for_interface(interface_name: &str) -> Result<Ipv4Addr, Box<dyn std::error::Error>> {
    let ifaces = get_if_addrs()?;
    for iface in ifaces {
        if iface.name == interface_name {
            if let IpAddr::V4(ipv4) = iface.ip() {
                return Ok(ipv4);
            }
        }
    }
    Err(format!("No IPv4 address found for interface '{}'", interface_name).into())
}

pub fn join_multicast_on_iface(
    multicast_addr: &str,
    port: u16,
    interface_name: &str,
) -> Result<Socket, Box<dyn std::error::Error>> {
    let group_v4: Ipv4Addr = multicast_addr.parse()?;
    let local_iface_ip = find_ipv4_for_interface(interface_name)?;

    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;

    let bind_addr = SocketAddr::new(std::net::IpAddr::V4(local_iface_ip), port);
    sock.bind(&bind_addr.into())?;

    sock.set_multicast_if_v4(&local_iface_ip)?;
    sock.join_multicast_v4(&group_v4, &local_iface_ip)?;

    println!(
        "Joined multicast group {} on interface {}, local IP: {}, port {}",
        multicast_addr, interface_name, local_iface_ip, port
    );
    Ok(sock)
}

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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

// ------------- DISKLESS MODE SUPPORT -------------

#[derive(Clone)]
struct InMemorySegment {
    data: Vec<u8>,
    duration: f64,
}

struct DisklessBuffer {
    queue: VecDeque<InMemorySegment>,
    max_segments: usize,
}

impl DisklessBuffer {
    fn new(max_segments: usize) -> Self {
        DisklessBuffer {
            queue: VecDeque::new(),
            max_segments,
        }
    }

    fn push_segment(&mut self, seg: InMemorySegment) {
        debug!(
            "Pushing segment into ring buffer, length={}, dur={}",
            seg.data.len(),
            seg.duration
        );
        self.queue.push_back(seg);
        if self.max_segments > 0 {
            while self.queue.len() > self.max_segments {
                self.queue.pop_front();
            }
        }
    }
}

async fn upload_memory_segment_to_s3(
    s3_client: &Client,
    bucket: &str,
    object_key: &str,
    segment_data: &[u8],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    debug!(
        "Starting S3 upload of in-memory segment to object key='{}', length={}",
        object_key,
        segment_data.len()
    );
    let body_stream = ByteStream::from(segment_data.to_vec());
    s3_client
        .put_object()
        .bucket(bucket)
        .key(object_key)
        .body(body_stream)
        .content_type("video/mp2t")
        .send()
        .await?;
    debug!("Completed S3 upload of object key='{}'", object_key);
    Ok(())
}

struct PlaylistEntry {
    duration: f64,
    path: String,
}

struct ManualSegmenter {
    output_dir: String,
    current_ts_file: Option<BufWriter<fs::File>>,
    segment_index: u64,
    playlist_path: PathBuf,
    m3u8_initialized: bool,

    max_segments_in_index: usize,
    playlist_entries: Vec<PlaylistEntry>,

    diskless_mode: bool,
    diskless_buffer: Arc<Mutex<DisklessBuffer>>,

    segment_open_time: Option<Instant>,
    bytes_this_segment: u64,
    last_measured_bitrate: f64,

    current_segment_buffer: Vec<u8>,

    s3_client: Option<Client>,
    s3_bucket: Option<String>,
    generate_unsigned_urls: bool,
    s3_endpoint: Option<String>,

    // If zero => no forced split
    diskless_max_bytes: usize,

    hourly_index_creator: Option<Arc<Mutex<HourlyIndexCreator>>>,
}

impl ManualSegmenter {
    fn new(output_dir: &str) -> Self {
        let playlist_path = Path::new("").join("index.m3u8");
        Self {
            output_dir: output_dir.to_string(),
            current_ts_file: None,
            segment_index: 0,
            playlist_path,
            m3u8_initialized: false,
            max_segments_in_index: 0,
            playlist_entries: Vec::new(),
            diskless_mode: false,
            diskless_buffer: Arc::new(Mutex::new(DisklessBuffer::new(0))),
            segment_open_time: None,
            bytes_this_segment: 0,
            last_measured_bitrate: 0.0,
            current_segment_buffer: Vec::new(),
            s3_client: None,
            s3_bucket: None,
            generate_unsigned_urls: false,
            s3_endpoint: None,
            diskless_max_bytes: get_max_segment_size_bytes(),
            hourly_index_creator: None,
        }
    }

    fn with_s3(
        mut self,
        s3_client: Option<Client>,
        s3_bucket: Option<String>,
        generate_unsigned_urls: bool,
        s3_endpoint: Option<String>,
    ) -> Self {
        self.s3_client = s3_client;
        self.s3_bucket = s3_bucket;
        self.generate_unsigned_urls = generate_unsigned_urls;
        self.s3_endpoint = s3_endpoint;
        self
    }

    fn with_max_segments(mut self, max_segments: usize) -> Self {
        self.max_segments_in_index = max_segments;
        self
    }

    fn with_diskless_mode(mut self, diskless: bool, ring_size: usize) -> Self {
        self.diskless_mode = diskless;
        self.diskless_buffer = Arc::new(Mutex::new(DisklessBuffer::new(ring_size)));
        self
    }

    fn with_diskless_max_bytes(mut self, max_bytes: usize) -> Self {
        self.diskless_max_bytes = max_bytes;
        self
    }

    fn with_hourly_index_creator(mut self, hic: Option<Arc<Mutex<HourlyIndexCreator>>>) -> Self {
        self.hourly_index_creator = hic;
        self
    }

    async fn write_ts(&mut self, data: &[u8]) -> std::io::Result<()> {
        // If this is our first data in a new segment, note the wall-clock start time
        if self.segment_open_time.is_none() {
            self.segment_open_time = Some(Instant::now());
        }

        // If not diskless, write to an actual file
        if !self.diskless_mode {
            if self.current_ts_file.is_none() {
                self.open_new_segment_file()?;
            }
            if let Some(file) = self.current_ts_file.as_mut() {
                file.write_all(data)?;
                file.flush()?;
            }
        } else {
            // If diskless, accumulate in self.current_segment_buffer
            self.current_segment_buffer.extend_from_slice(data);

            // If user set a max size for each segment, forcibly cut once we exceed it
            if self.diskless_max_bytes > 0
                && self.current_segment_buffer.len() >= self.diskless_max_bytes
            {
                info!(
                    "[DISKLESS] buffer >= {} bytes, forcing segment close...",
                    self.diskless_max_bytes
                );
                self.close_current_segment_file().await?;
                if !self.diskless_mode {
                    self.open_new_segment_file()?;
                }
            }
        }

        // Just counting bytes for fallback bitrate
        self.bytes_this_segment += data.len() as u64;

        // We'll do a simple "close if wall clock or byte-based logic says so"
        let desired_secs = get_segment_duration_seconds() as f64;
        let now = Instant::now();
        let elapsed_wall = self
            .segment_open_time
            .map(|st| now.duration_since(st).as_secs_f64())
            .unwrap_or(0.0);

        let mut enough_bytes_based_on_bitrate = false;
        if self.last_measured_bitrate > 0.0 {
            let needed_bytes = self.last_measured_bitrate * desired_secs;
            if self.bytes_this_segment as f64 >= needed_bytes {
                enough_bytes_based_on_bitrate = true;
            }
        }

        let fallback_time_expired = elapsed_wall >= desired_secs * 1.5;

        if elapsed_wall >= desired_secs || enough_bytes_based_on_bitrate || fallback_time_expired {
            info!(
                  "Trigger close segment: using wall-clock. elapsed_wall={:.2}, desired_secs={}, enough_bytes_based_on_bitrate={}, fallback_time_expired={}",
                  elapsed_wall,
                  desired_secs,
                  enough_bytes_based_on_bitrate,
                  fallback_time_expired
              );
            self.close_current_segment_file().await?;
            if !self.diskless_mode {
                self.open_new_segment_file()?;
            }
        }

        Ok(())
    }

    async fn close_current_segment_file(&mut self) -> std::io::Result<()> {
        // Measure real_elapsed from segment_open_time
        let mut real_elapsed = if get_use_estimated_duration() {
            self.segment_open_time
                .map(|st| Instant::now().duration_since(st).as_secs_f64())
                .unwrap_or(0.0)
        } else {
            let d = get_segment_duration_seconds() as f64;
            d + 1.0
        };

        // round up real_elapsed to nearest second
        real_elapsed = real_elapsed.ceil();

        debug!(
            "Closing segment {}, measured wall-clock duration={:.3}s",
            self.segment_index + 1,
            real_elapsed
        );

        // If not diskless, finalize file-based approach
        if !self.diskless_mode {
            if let Some(mut writer) = self.current_ts_file.take() {
                writer.flush()?;
                drop(writer);

                // Write sidecar
                let segment_path = self.current_segment_path(self.segment_index + 1);
                let duration_path = segment_path.with_extension("dur");
                let full_dur_path = Path::new(&self.output_dir).join(&duration_path);
                fs::write(full_dur_path, format!("{:.3}", real_elapsed))?;

                // Add to playlist
                self.playlist_entries.push(PlaylistEntry {
                    duration: real_elapsed,
                    path: format!("{}/{}", self.output_dir, segment_path.to_string_lossy()),
                });

                if self.max_segments_in_index > 0
                    && self.playlist_entries.len() > self.max_segments_in_index
                {
                    let removed = self.playlist_entries.remove(0);
                    let old_path = Path::new(&removed.path);
                    if old_path.exists() {
                        let _ = fs::remove_file(old_path);
                        let dur_path = old_path.with_extension("dur");
                        let _ = fs::remove_file(dur_path);
                    }
                }
            }
        } else {
            // diskless mode => finalize the chunk in memory
            let seg_data_clone = self.current_segment_buffer.clone();
            self.current_segment_buffer.clear();
            info!(
                "[DISKLESS] Finalizing seg#{} in memory, length={}, wall-clock dur={:.3}",
                self.segment_index + 1,
                seg_data_clone.len(),
                real_elapsed
            );

            {
                let mut buf = self.diskless_buffer.lock().await;
                buf.push_segment(InMemorySegment {
                    data: seg_data_clone.clone(),
                    duration: real_elapsed,
                });
            }

            // Optionally upload to S3
            let mut final_path = format!("mem://segment_{}", self.segment_index + 1);
            if let (Some(ref s3c), Some(ref buck)) = (&self.s3_client, &self.s3_bucket) {
                let segment_path = self.current_segment_path(self.segment_index + 1);
                let object_key = format!("{}", segment_path.to_string_lossy());
                info!(
                    "[DISKLESS] Attempt S3 upload of object_key={}, len={}",
                    object_key,
                    seg_data_clone.len()
                );

                match upload_memory_segment_to_s3(s3c, buck, &object_key, &seg_data_clone).await {
                    Ok(_) => {
                        debug!("[DISKLESS] S3 upload succeeded. Now build URL...");
                        if self.generate_unsigned_urls {
                            if let Some(ref endpoint) = self.s3_endpoint {
                                final_path = format!("{}/{}/{}", endpoint, buck, object_key);
                            }
                        } else {
                            match self.generate_s3_url(&object_key).await {
                                Ok(url_str) => {
                                    final_path = url_str;
                                }
                                Err(e) => {
                                    warn!(
                                        "[DISKLESS] presign failed: {:?}, fallback to mem:// path",
                                        e
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            "[DISKLESS] S3 upload of object_key='{}' failed: {:?}",
                            object_key, e
                        );
                    }
                }
            }

            // Add to playlist
            self.playlist_entries.push(PlaylistEntry {
                duration: real_elapsed,
                path: final_path,
            });

            if self.max_segments_in_index > 0
                && self.playlist_entries.len() > self.max_segments_in_index
            {
                let _ = self.playlist_entries.remove(0);
            }
        }

        // In diskless mode, notify HourlyIndexCreator about this segment
        if self.diskless_mode {
            if let (Some(ref hic_arc), Some(_buck)) = (&self.hourly_index_creator, &self.s3_bucket)
            {
                let now = chrono::Local::now();
                let year = now.format("%Y").to_string();
                let month = now.format("%m").to_string();
                let day = now.format("%d").to_string();
                let hour = now.format("%H").to_string();

                let hour_dir = format!("{}/{}/{}/{}", year, month, day, hour);
                let object_key = format!(
                    "{}",
                    self.current_segment_path(self.segment_index + 1)
                        .to_string_lossy()
                );
                let custom_lines = vec![];

                let mut guard = hic_arc.lock().await;
                if let Err(e) = guard
                    .record_segment(
                        &hour_dir,
                        &object_key,
                        real_elapsed,
                        custom_lines,
                        &self.output_dir,
                    )
                    .await
                {
                    warn!("Failed to record segment in diskless mode: {:?}", e);
                }
            }
        }

        // bump segment index
        self.segment_index += 1;

        // rewrite local M3U8
        if let Err(e) = self.rewrite_m3u8() {
            warn!("Error rewriting m3u8: {:?}", e);
        }

        // measure overall "bitrate"
        if let Some(st) = self.segment_open_time.take() {
            let real_elapsed = Instant::now().duration_since(st).as_secs_f64();
            if real_elapsed > 0.2 {
                let segment_bitrate = self.bytes_this_segment as f64 / real_elapsed;
                self.last_measured_bitrate =
                    0.6 * self.last_measured_bitrate + 0.4 * segment_bitrate;
            }
        }

        // done, reset counters
        self.bytes_this_segment = 0;
        Ok(())
    }

    async fn generate_s3_url(
        &self,
        object_key: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        if self.generate_unsigned_urls {
            if let (Some(ref endpoint), Some(ref buck)) = (&self.s3_endpoint, &self.s3_bucket) {
                Ok(format!("{}/{}/{}", endpoint, buck, object_key))
            } else {
                Err("missing endpoint or bucket for generate_s3_url".into())
            }
        } else {
            if let (Some(ref client), Some(ref buck)) = (&self.s3_client, &self.s3_bucket) {
                let config =
                    PresigningConfig::expires_in(Duration::from_secs(get_url_signing_seconds()))?;
                let presigned_req = client
                    .get_object()
                    .bucket(buck)
                    .key(object_key)
                    .presigned(config)
                    .await?;
                Ok(presigned_req.uri().to_string())
            } else {
                Err("No s3_client or s3_bucket for presigned url".into())
            }
        }
    }

    fn open_new_segment_file(&mut self) -> std::io::Result<()> {
        if self.diskless_mode {
            return Ok(());
        }
        let segment_path = self.current_segment_path(self.segment_index);
        let full_path = Path::new(&self.output_dir).join(&segment_path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = fs::File::create(&full_path)?;
        let writer = BufWriter::new(file);

        self.current_ts_file = Some(writer);
        if !self.m3u8_initialized {
            self.init_m3u8()?;
            self.m3u8_initialized = true;
        }
        Ok(())
    }

    fn init_m3u8(&self) -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.playlist_path)?;

        writeln!(f, "#EXTM3U")?;
        writeln!(f, "#EXT-X-VERSION:3")?;
        writeln!(
            f,
            "#EXT-X-TARGETDURATION:{}",
            get_segment_duration_seconds() + 1
        )?;
        writeln!(f, "#EXT-X-MEDIA-SEQUENCE:0")?;
        Ok(())
    }

    fn rewrite_m3u8(&self) -> std::io::Result<()> {
        debug!(
            "Rewriting m3u8 with {} entries",
            self.playlist_entries.len()
        );
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.playlist_path)?;

        writeln!(f, "#EXTM3U")?;
        writeln!(f, "#EXT-X-VERSION:3")?;

        let max_seg = self
            .playlist_entries
            .iter()
            .map(|e| e.duration.ceil() as u64)
            .max()
            .unwrap_or_else(|| get_segment_duration_seconds());
        writeln!(f, "#EXT-X-TARGETDURATION:{}", max_seg)?;

        let seq_start = self
            .segment_index
            .saturating_sub(self.playlist_entries.len() as u64);
        writeln!(f, "#EXT-X-MEDIA-SEQUENCE:{}", seq_start)?;

        for entry in &self.playlist_entries {
            writeln!(f, "#EXTINF:{:.6},", entry.duration)?;
            writeln!(f, "{}", entry.path)?;
        }
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
        let _ = tokio::runtime::Handle::try_current().map(|handle| {
            handle.block_on(async {
                let _ = self.close_current_segment_file().await;
            })
        });
    }
}

// ------------- MAIN -------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .format_timestamp_secs()
        .init();
    debug!("Logging initialized. Starting main()...");

    let matches = ClapCommand::new("mpegts_to_s3")
        .version(get_version())
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
                .default_value("us-east-1")
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
                .default_value("ts")
                .help("Local dir for HLS output"),
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
            Arg::new("hls_keep_segments")
                .long("hls_keep_segments")
                .help("Limit how many segments to keep in index.m3u8 (0=unlimited).")
                .default_value("10"),
        )
        .arg(
            Arg::new("unsigned_urls")
                .long("unsigned_urls")
                .help("Generate unsigned S3 URLs instead of presigned URLs.")
                .action(clap::ArgAction::SetTrue),
        )
        // diskless mode
        .arg(
            Arg::new("diskless_mode")
                .long("diskless_mode")
                .help("Keep TS segments in memory instead of writing them to disk.")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("diskless_ring_size")
                .long("diskless_ring_size")
                .help("Number of diskless segments to keep in memory ring buffer.")
                .default_value("1"),
        )
        .arg(
            Arg::new("encode")
                .long("encode")
                .help("Encode the video stream to H.264/AAC using FFmpeg.")
                .action(clap::ArgAction::SetTrue),
        )
        .get_matches();

    debug!("Command-line arguments parsed: {:?}", matches);

    let endpoint = matches.get_one::<String>("endpoint").unwrap();
    let region_name = matches.get_one::<String>("region").unwrap();
    let bucket = matches.get_one::<String>("bucket").unwrap();
    let filter_ip = matches.get_one::<String>("udp_ip").unwrap();
    let filter_port: u16 = matches.get_one::<String>("udp_port").unwrap().parse()?;
    let interface = matches.get_one::<String>("interface").unwrap();
    let timeout: i32 = matches.get_one::<String>("timeout").unwrap().parse()?;
    let output_dir = matches.get_one::<String>("output_dir").unwrap();
    let remove_local = matches.get_flag("remove_local");
    let mut manual_segment = matches.get_flag("manual_segment");
    let generate_unsigned_urls = matches.get_flag("unsigned_urls");
    let encode = matches.get_flag("encode");
    let diskless_mode = matches.get_flag("diskless_mode");

    if diskless_mode {
        manual_segment = true;
    }

    if manual_segment && encode {
        eprintln!("Cannot use --manual_segment and --encode together.");
        std::process::exit(1);
    }

    let hls_keep_segments: usize = matches
        .get_one::<String>("hls_keep_segments")
        .unwrap()
        .parse()
        .unwrap_or(10);

    let diskless_ring_size: usize = matches
        .get_one::<String>("diskless_ring_size")
        .unwrap()
        .parse()
        .unwrap_or(1);

    info!(
        "MpegTS to S3: endpoint={}, region={}, bucket={}, udp_ip={}, udp_port={}, \
                 interface={}, timeout={}, output_dir={}, remove_local={}, manual_segment={}, \
                 hls_keep_segments={}, generate_unsigned_urls={}, \
                 diskless_mode={}, diskless_ring_size={} encode={}",
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
        hls_keep_segments,
        generate_unsigned_urls,
        diskless_mode,
        diskless_ring_size,
        encode
    );

    fs::create_dir_all(output_dir)?;

    info!("Initializing S3 client with endpoint: {}", endpoint);
    let creds = Credentials::new(get_s3_username(), get_s3_password(), None, None, "dummy");

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

    let hourly_index_creator = HourlyIndexCreator::new(
        s3_client.clone(),
        bucket.to_string(),
        generate_unsigned_urls,
        endpoint.clone(),
    );
    // Wrap it in Arc<tokio::sync::Mutex<...>>
    let hourly_index_creator = Arc::new(Mutex::new(hourly_index_creator));

    info!("Starting directory watcher on: {}", output_dir);
    let (watch_tx, watch_rx) = channel();
    let watch_dir = output_dir.to_string();
    let watch_thread = thread::spawn(move || {
        if let Err(e) = watch_directory(&watch_dir, watch_tx) {
            eprintln!("Directory watcher error: {:?}", e);
        }
    });

    let (mut ffmpeg_child, mut ffmpeg_stdin_buf) = if !manual_segment {
        let hls_segment_filename =
            format!("{}/%Y/%m/%d/%H/segment_%Y%m%d-%H%M%S_%04d.ts", output_dir);
        let m3u8_output = format!("index.m3u8");

        info!("Starting FFmpeg with output: {}", m3u8_output);
        let mut child = Command::new("ffmpeg")
            .arg("-analyzeduration")
            .arg("1000000")
            .arg("-probesize")
            .arg("1000000")
            .arg("-f")
            .arg("mpegts")
            .arg("-i")
            .arg("pipe:0")
            .arg("-c:v")
            .arg(if encode { "h264" } else { "copy" })
            .arg("-c:a")
            .arg(if encode { "aac" } else { "copy" })
            .arg("-loglevel")
            .arg("info")
            .arg("-y")
            .arg("-hide_banner")
            //.arg("-nostats")
            .arg("-max_delay")
            .arg("0")
            .arg("-f")
            .arg("hls")
            .arg(if encode { "" } else { "-map" })
            .arg(if encode { "" } else { "0" })
            // smaller segments
            .arg("-hls_time")
            .arg(get_segment_duration_seconds().to_string())
            .arg("-hls_segment_type")
            .arg("mpegts")
            .arg("-hls_playlist_type")
            .arg("event")
            .arg("-hls_list_size")
            .arg(hls_keep_segments.to_string())
            .arg("-strftime")
            .arg("1")
            .arg("-strftime_mkdir")
            .arg("1")
            .arg("-hls_segment_filename")
            .arg(&hls_segment_filename)
            .arg(&m3u8_output)
            .stdin(Stdio::piped())
            //.stdout(Stdio::null())
            //.stderr(Stdio::piped())
            .spawn()?;

        let child_stdin = child.stdin.take().ok_or("Failed to take FFmpeg stdin")?;
        let buf = BufWriter::new(child_stdin);

        (Some(child), Some(buf))
    } else {
        (None, None)
    };

    let s3_client_clone = s3_client.clone();
    let bucket_clone = bucket.clone();
    let output_dir_clone = output_dir.clone();
    let output_dir_for_task = output_dir.clone();

    let hic_for_task = hourly_index_creator.clone();
    let upload_task = tokio::spawn(async move {
        if let Err(e) = handle_file_events(
            watch_rx,
            s3_client_clone,
            bucket_clone,
            output_dir_clone,
            remove_local,
            hic_for_task,
            output_dir_for_task,
        )
        .await
        {
            error!("File event handler error: {}", e);
        }
    });

    debug!(
        "Attempting to join multicast {}:{} on interface={}",
        filter_ip, filter_port, interface
    );
    let _sock = match join_multicast_on_iface(filter_ip, filter_port, interface) {
        Ok(s) => {
            debug!("Successfully joined multicast group");
            s
        }
        Err(e) => {
            eprintln!("Failed to join multicast group or find interface IP: {}", e);
            return Ok(());
        }
    };

    debug!(
        "Opening PCAP on interface={} with snaplen=65535, buffer=8MB, timeout={}",
        interface, timeout
    );

    let mut cap = Capture::from_device(interface.as_str())?
        .promisc(true)
        .buffer_size(get_buffer_size())
        .snaplen(get_snaplen())
        .timeout(timeout)
        .open()?;

    let filter_expr = format!("udp and host {} and port {}", filter_ip, filter_port);
    debug!("Setting pcap filter to '{}'", filter_expr);
    cap.filter(&filter_expr, true)?;

    println!(
        "Capturing on '{}', listening for {}:{}, writing HLS to '{}'",
        interface, filter_ip, filter_port, output_dir
    );

    let mut manual_segmenter = if manual_segment {
        Some(
            ManualSegmenter::new(output_dir)
                .with_max_segments(hls_keep_segments)
                .with_diskless_mode(diskless_mode, diskless_ring_size)
                .with_s3(
                    Some(s3_client.clone()),
                    Some(bucket.clone()),
                    generate_unsigned_urls,
                    Some(endpoint.clone()),
                )
                .with_diskless_max_bytes(get_max_segment_size_bytes())
                .with_hourly_index_creator(Some(hourly_index_creator.clone())),
        )
    } else {
        None
    };

    let diskless_consumer = if diskless_mode {
        if let Some(_seg_ref) = &manual_segmenter {
            let buffer_ref = _seg_ref.diskless_buffer.clone();
            let handle = thread::spawn(move || loop {
                {
                    // This is a plain thread, but it's only popping from
                    // the tokio::sync::Mutex in a blocking way.
                    let mut buf = futures::executor::block_on(buffer_ref.lock());
                    if let Some(front) = buf.queue.pop_front() {
                        debug!(
                            "Diskless consumer got a segment of len {}, dur={}",
                            front.data.len(),
                            front.duration
                        );
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            });
            Some(handle)
        } else {
            None
        }
    } else {
        None
    };

    debug!("Starting main capture loop now...");
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
                // Possibly no packet arrived in the last 'timeout' ms
                // so check if we want to forcibly close a segment on a timer, etc.
                if let Some(_seg) = manual_segmenter.as_mut() {
                    if let Err(e) = _seg.close_current_segment_file().await {
                        eprintln!("Error closing segment: {:?}", e);
                        break;
                    }
                }
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
            if let Some(buf) = ffmpeg_stdin_buf.as_mut() {
                if let Err(e) = buf.write_all(ts_payload) {
                    eprintln!("Error feeding data to FFmpeg: {:?}", e);
                    break;
                }
            }

            if let Some(seg) = manual_segmenter.as_mut() {
                if let Err(e) = seg.write_ts(ts_payload).await {
                    eprintln!("Error writing manual TS segment: {:?}", e);
                    break;
                }
            }
        } else {
            debug!(
                "extract_mpegts_payload returned None => not recognized as TS for {}:{}",
                filter_ip, filter_port
            );
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

    if let Some(handle) = diskless_consumer {
        let _ = handle.thread().id();
    }

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
    hourly_index_creator: Arc<Mutex<HourlyIndexCreator>>,
    output_dir: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Also use tokio::sync::Mutex for the tracker
    let tracker = Arc::new(Mutex::new(FileTracker::new(get_file_max_age_seconds())));

    while let Ok(event) = rx.recv() {
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                for path in event.paths {
                    if let Some(ext) = path.extension() {
                        if (ext == "ts" || ext == "m3u8")
                            && !path.to_string_lossy().contains("_temp.")
                        {
                            let full_path_str = path.to_string_lossy().to_string();

                            // We lock the tracker to see if we have uploaded or if it's too old
                            {
                                let tr = tracker.lock().await;
                                if tr.is_uploaded(&full_path_str) || tr.is_too_old(&path) {
                                    // Already handled or too old
                                    continue;
                                }
                            }

                            // If it's a normal file (not "mem://..."), wait for it to stabilize
                            if !full_path_str.starts_with("mem://") {
                                let mut retries = 30;
                                let mut stable_count = 0;
                                let mut last_size = 0;

                                while retries > 0 && stable_count < 3 {
                                    if let Ok(metadata) = fs::metadata(&path) {
                                        let current_size = metadata.len();
                                        if current_size > 0 {
                                            if current_size == last_size {
                                                stable_count += 1;
                                            } else {
                                                stable_count = 0;
                                                last_size = current_size;
                                            }
                                        }
                                    }
                                    sleep(Duration::from_millis(300)).await;
                                    retries -= 1;
                                }

                                if stable_count < 3 {
                                    warn!("File {} did not stabilize, skipping", full_path_str);
                                    continue;
                                }
                            }

                            let relative_path = match strip_base_dir(&path, &base_dir) {
                                Ok(rp) => rp,
                                Err(e) => {
                                    warn!(
                                        "Skipping {}: strip_base_dir() error: {}",
                                        full_path_str, e
                                    );
                                    continue;
                                }
                            };
                            let key_str = relative_path.to_string_lossy().to_string();
                            let hour_dir = relative_path
                                .parent()
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_else(|| "".to_string());

                            if let Ok(()) = upload_file_to_s3(
                                &s3_client,
                                &bucket,
                                &base_dir,
                                &path,
                                remove_local,
                            )
                            .await
                            {
                                // Mark uploaded
                                let mut tr = tracker.lock().await;
                                tr.mark_uploaded(full_path_str.clone());
                            }

                            // Compute the segment duration
                            let pcr_dur_path = path.with_extension("dur");
                            let actual_segment_duration = if pcr_dur_path.exists() {
                                if let Ok(dur_str) = fs::read_to_string(&pcr_dur_path) {
                                    dur_str
                                        .parse()
                                        .unwrap_or_else(|_| get_actual_file_duration(&path))
                                } else {
                                    get_actual_file_duration(&path)
                                }
                            } else {
                                get_actual_file_duration(&path)
                            };

                            let custom_lines = vec![];

                            // Lock HourlyIndexCreator, update segment info
                            {
                                let mut hic = hourly_index_creator.lock().await;
                                let _ = hic
                                    .record_segment(
                                        &hour_dir,
                                        &key_str,
                                        actual_segment_duration,
                                        custom_lines,
                                        &output_dir,
                                    )
                                    .await;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

async fn upload_file_to_s3(
    s3_client: &Client,
    bucket: &str,
    base_dir: &str,
    path: &Path,
    remove_local: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let relative_path = strip_base_dir(path, base_dir)?;
    let key_str = relative_path.to_string_lossy().to_string();

    let mime_type = if let Some(ext) = path.extension() {
        if ext == "m3u8" {
            "application/vnd.apple.mpegurl"
        } else {
            "video/mp2t"
        }
    } else {
        "application/octet-stream"
    };

    let file_size = fs::metadata(path)?.len();
    println!(
        "Uploading {} ({} bytes) -> s3://{}/{} as {}",
        path.display(),
        file_size,
        bucket,
        key_str,
        mime_type
    );

    let mut retries = 3;
    while retries > 0 {
        let metadata_size = fs::metadata(path)?.len();
        let file_contents = fs::read(path)?;
        let read_size = file_contents.len();

        println!(
            "File checks for {}\n  Metadata size: {}\n  Actual read size: {}",
            path.display(),
            metadata_size,
            read_size
        );

        let body_stream = ByteStream::from(file_contents);

        println!(
            "Uploading {} (metadata: {} bytes, read: {} bytes) -> s3://{}/{}",
            path.display(),
            metadata_size,
            read_size,
            bucket,
            key_str
        );
        match s3_client
            .put_object()
            .bucket(bucket)
            .key(&key_str)
            .body(body_stream)
            .content_type(mime_type)
            .content_length(file_size as i64)
            .send()
            .await
        {
            Ok(_) => {
                println!("Uploaded {} ({} bytes)", key_str, file_size);
                if remove_local {
                    if let Err(e) = fs::remove_file(path) {
                        eprintln!("Failed removing local file {}: {:?}", path.display(), e);
                    }
                    let dur_sidecar = path.with_extension("dur");
                    if dur_sidecar.exists() {
                        if let Err(e) = fs::remove_file(&dur_sidecar) {
                            eprintln!("Failed removing local sidecar {:?}: {:?}", dur_sidecar, e);
                        }
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
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
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
            "Unknown payload type found (expected TS or RTP-TS). First byte=0x{:02x}, size={}",
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
