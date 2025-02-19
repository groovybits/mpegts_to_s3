/*
 * udp-to-hls
 *
 * This program captures MPEG-TS packets from a UDP multicast stream:
 *   Segments the MPEG-TS and writes an .m3u8 playlist, uploads to an S3 bucket as HLS.
 *
 * Chris Kennedy 2025 Jan 15
 */

use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use aws_types::region::Region;
use clap::{Arg, Command as ClapCommand};
use ctrlc;
use env_logger;
use futures::StreamExt;
use get_if_addrs::get_if_addrs;
use log::{debug, error, info, warn};
use mpegts_pid_tracker::{PidTracker, TS_PACKET_SIZE};
use pcap::{Capture, PacketCodec};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

// ------------- HELPER STRUCTS & FUNCS -------------

fn get_segment_duration_ms() -> f64 {
    std::env::var("SEGMENT_DURATION_MS")
        .unwrap_or_else(|_| "2000.0".to_string())
        .parse()
        .unwrap_or(2000.0)
}

fn get_max_segment_size_bytes() -> usize {
    std::env::var("MAX_SEGMENT_SIZE_BYTES")
        .unwrap_or_else(|_| "5242756".to_string())
        .parse()
        .unwrap_or(5242756)
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
        .unwrap_or_else(|_| TS_PACKET_SIZE.to_string())
        .parse()
        .unwrap_or(TS_PACKET_SIZE)
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
    std::env::var("CAPTURE_BUFFER_SIZE")
        .unwrap_or_else(|_| "1048476".to_string())
        .parse()
        .unwrap_or(1048476)
}

fn get_use_estimated_duration() -> bool {
    std::env::var("USE_ESTIMATED_DURATION")
        .unwrap_or_else(|_| "false".to_string())
        .parse()
        .unwrap_or(false)
}

fn get_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ------------- HOURLY INDEX CREATOR -------------
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
        let index_path = local_dir.join("index.m3u8");
        let temp_path = local_dir.join("index_temp.m3u8");

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

            let target_duration_secs = if let Some(vec) = entries {
                let max_seg = vec
                    .iter()
                    .map(|e| e.duration.ceil() as u64)
                    .max()
                    .unwrap_or(get_segment_duration_ms() as u64);
                max_seg
            } else {
                get_segment_duration_ms() as u64 / 1000
            };
            writeln!(file, "#EXT-X-TARGETDURATION:{}", target_duration_secs)?;

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

        let s3_object_path = format!("{}/{}", output_dir, hour_dir);

        self.upload_local_file_to_s3(
            &index_path,
            &format!("{}/index.m3u8", s3_object_path),
            "application/vnd.apple.mpegurl",
        )
        .await?;

        let final_index_url = self
            .presign_get_url(&format!("{}/index.m3u8", s3_object_path))
            .await?;
        self.rewrite_urls_log(&s3_object_path, &final_index_url)?;
        Ok(())
    }

    fn rewrite_urls_log(&mut self, hour_dir: &str, final_url: &str) -> std::io::Result<()> {
        let log_path = std::path::Path::new("").join("hourly_urls.log");
        let temp_path = std::path::Path::new("").join("hourly_urls_temp.log");

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
        log::info!(
              "UDPtoHLS: [upload_local_file_to_s3()] Uploading hourly index -> s3://{}/{} (content_type={})",
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
            if let std::net::IpAddr::V4(ipv4) = iface.ip() {
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
        "UDPtoHLS: Joined multicast group {} on interface {}, local IP: {}, port {}",
        multicast_addr, interface_name, local_iface_ip, port
    );
    Ok(sock)
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
    segment_index: u64,
    playlist_path: PathBuf,

    max_segments_in_index: usize,
    playlist_entries: Vec<PlaylistEntry>,

    diskless_buffer: Arc<Mutex<DisklessBuffer>>,

    segment_open_time: Option<Instant>,
    segment_start_time: Option<chrono::DateTime<chrono::Local>>,
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
        let playlist_file = format!("{}.m3u8", output_dir);
        let playlist_path = Path::new("").join(&playlist_file);
        Self {
            output_dir: output_dir.to_string(),
            segment_index: 0,
            playlist_path,
            max_segments_in_index: 0,
            playlist_entries: Vec::new(),
            diskless_buffer: Arc::new(Mutex::new(DisklessBuffer::new(0))),
            segment_open_time: None,
            segment_start_time: None,
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

    pub async fn finalize(&mut self) -> std::io::Result<()> {
        self.close_current_segment_file().await
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

    fn with_diskless_ring_size(mut self, ring_size: usize) -> Self {
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

    async fn write_ts(&mut self, timestamp: u64, data: &[u8]) -> std::io::Result<()> {
        //let ts_instant = SystemTime::UNIX_EPOCH + Duration::from_millis(timestamp);
        //let instant = Instant::now() - SystemTime::now().duration_since(ts_instant).unwrap();

        log::debug!(
            "Writing TS packet, len={}, timestamp={}",
            data.len(),
            timestamp
        );

        if self.segment_open_time.is_none() {
            self.segment_open_time = Some(Instant::now());
            self.segment_start_time = Some(chrono::Local::now());
        }

        self.current_segment_buffer.extend_from_slice(data);
        if self.diskless_max_bytes > 0
            && self.current_segment_buffer.len() >= self.diskless_max_bytes
        {
            info!(
                "[DISKLESS] buffer >= {} bytes, forcing segment close...",
                self.diskless_max_bytes
            );
            self.close_current_segment_file().await?;
        }

        self.bytes_this_segment += data.len() as u64;

        let desired_ms = get_segment_duration_ms() as f64;
        let desired_secs = desired_ms / 1000.0;
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
        }

        Ok(())
    }

    async fn close_current_segment_file(&mut self) -> std::io::Result<()> {
        let mut real_elapsed = if get_use_estimated_duration() {
            self.segment_open_time
                .map(|st| Instant::now().duration_since(st).as_secs_f64())
                .unwrap_or(0.0)
        } else {
            let d = get_segment_duration_ms() as f64;
            d / 1000.0
        };

        real_elapsed = real_elapsed.ceil();

        debug!(
            "Closing segment {}, measured wall-clock duration={:.3}s",
            self.segment_index + 1,
            real_elapsed
        );

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

        let mut final_path = format!("mem://segment_{}", self.segment_index + 1);
        if let (Some(ref s3c), Some(ref buck)) = (&self.s3_client, &self.s3_bucket) {
            let segment_path = self.current_segment_path(self.segment_index + 1);
            let object_key = format!("{}/{}", self.output_dir, segment_path.to_string_lossy());
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
                                error!(
                                    "[DISKLESS] presign failed: {:?}, fallback to mem:// path",
                                    e
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    error!(
                        "[DISKLESS] S3 upload of object_key='{}' failed: {:?}",
                        object_key, e
                    );
                }
            }
        }

        self.playlist_entries.push(PlaylistEntry {
            duration: real_elapsed,
            path: final_path,
        });

        if self.max_segments_in_index > 0
            && self.playlist_entries.len() > self.max_segments_in_index
        {
            let _ = self.playlist_entries.remove(0);
        }

        if let (Some(ref hic_arc), Some(_buck)) = (&self.hourly_index_creator, &self.s3_bucket) {
            let now = chrono::Local::now();
            let year = now.format("%Y").to_string();
            let month = now.format("%m").to_string();
            let day = now.format("%d").to_string();
            let hour = now.format("%H").to_string();

            let hour_dir = format!("{}/{}/{}/{}", year, month, day, hour);
            let object_key = format!(
                "{}/{}",
                self.output_dir,
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

        self.segment_index += 1;

        self.segment_start_time = None;

        if let Err(e) = self.rewrite_m3u8() {
            warn!("Error rewriting m3u8: {:?}", e);
        }

        if let (Some(ref s3_client), Some(ref bucket_name)) = (&self.s3_client, &self.s3_bucket) {
            let s3_key = format!("{}/index.m3u8", self.output_dir);
            let local_m3u8 = std::path::Path::new("").join(format!("{}.m3u8", self.output_dir));

            if local_m3u8.exists() {
                match s3_client
                    .put_object()
                    .bucket(bucket_name)
                    .key(&s3_key)
                    //.acl(aws_sdk_s3::types::ObjectCannedAcl::PublicRead)
                    .body(ByteStream::from_path(&local_m3u8).await?)
                    .send()
                    .await
                {
                    Ok(_) => {
                        log::info!(
                            "Successfully uploaded index M3U8 to {}/{}",
                            bucket_name,
                            s3_key
                        );
                    }
                    Err(e) => {
                        log::error!("Failed to upload index M3U8 {}: {:?}", s3_key, e);
                    }
                }
            } else {
                log::error!(
                    "UDPtoHLS: Not uploading index M3U8: local file does not exist at {:?}",
                    local_m3u8
                );
            }
        }

        if let Some(st) = self.segment_open_time.take() {
            let real_elapsed = Instant::now().duration_since(st).as_secs_f64();
            if real_elapsed > 0.2 {
                let segment_bitrate = self.bytes_this_segment as f64 / real_elapsed;
                self.last_measured_bitrate =
                    0.6 * self.last_measured_bitrate + 0.4 * segment_bitrate;
            }
        }

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
                Err("UDPtoHLS: missing endpoint or bucket for generate_s3_url".into())
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
                Err("UDPtoHLS: No s3_client or s3_bucket for presigned url".into())
            }
        }
    }

    fn rewrite_m3u8(&self) -> std::io::Result<()> {
        debug!(
            "Rewriting m3u8 with {} entries",
            self.playlist_entries.len()
        );
        let tmp_path = self.playlist_path.with_file_name(format!(
            "{}_temp",
            self.playlist_path.file_name().unwrap().to_string_lossy()
        ));

        {
            let mut f = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)?;

            writeln!(f, "#EXTM3U")?;
            writeln!(f, "#EXT-X-VERSION:3")?;

            let max_seg_secs = self
                .playlist_entries
                .iter()
                .map(|e| e.duration.ceil() as u64)
                .max()
                .unwrap_or_else(|| get_segment_duration_ms() as u64 / 1000);
            writeln!(f, "#EXT-X-TARGETDURATION:{}", max_seg_secs)?;

            let seq_start = self
                .segment_index
                .saturating_sub(self.playlist_entries.len() as u64);
            writeln!(f, "#EXT-X-MEDIA-SEQUENCE:{}", seq_start)?;

            for entry in &self.playlist_entries {
                writeln!(f, "#EXTINF:{:.6},", entry.duration)?;
                writeln!(f, "{}", entry.path)?;
            }
        }

        fs::rename(&tmp_path, &self.playlist_path)?;
        Ok(())
    }

    fn current_segment_path(&self, index: u64) -> PathBuf {
        let now = self
            .segment_start_time
            .unwrap_or_else(|| chrono::Local::now());
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

// Define the BoxCodec for pcap streaming
pub struct BoxCodec;

impl PacketCodec for BoxCodec {
    type Item = (Box<[u8]>, SystemTime);

    fn decode(&mut self, packet: pcap::Packet) -> Self::Item {
        let timestamp = UNIX_EPOCH
            + Duration::new(
                packet.header.ts.tv_sec as u64,
                packet.header.ts.tv_usec as u32 * 1000,
            );
        (packet.data.into(), timestamp)
    }
}

// ------------- MAIN -------------
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let mut ctrl_counter = 0;
    ctrlc::set_handler({
        let shutdown_flag = Arc::clone(&shutdown_flag);
        move || {
            log::error!("UDPtoHLS: Got CTRL+C, shutting down gracefully...");
            shutdown_flag.store(true, Ordering::SeqCst);
            ctrl_counter += 1;
            if ctrl_counter >= 3 {
                log::error!("UDPtoHLS: Got CTRL+C 3 times, forcing exit.");
                std::process::exit(1);
            }
        }
    })
    .expect("Error setting Ctrl-C handler");
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
                .default_value("hls")
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
                .default_value("channel01")
                .help("Local dir for HLS output"),
        )
        .arg(
            Arg::new("remove_local")
                .long("remove_local")
                .help("Remove local .ts/.m3u8 after uploading to S3?")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("hls_keep_segments")
                .long("hls_keep_segments")
                .help("Limit how many segments to keep in index.m3u8 (0=unlimited).")
                .default_value("3"),
        )
        .arg(
            Arg::new("unsigned_urls")
                .long("unsigned_urls")
                .help("Generate unsigned S3 URLs instead of presigned URLs.")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("diskless_ring_size")
                .long("diskless_ring_size")
                .help("Number of diskless segments to keep in memory ring buffer.")
                .default_value("1"),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .default_value("0"),
        )
        .arg(
            Arg::new("quiet")
                .short('q')
                .long("quiet")
                .help("Suppress all non error output")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("pcap_stats_interval")
                .long("pcap_stats_interval")
                .default_value("30")
                .help("Interval in seconds to print PCAP stats"),
        )
        .arg(
            Arg::new("drop_corrupt_ts")
                .long("drop_corrupt_ts")
                .help("Drop corrupt TS packets")
                .action(clap::ArgAction::SetTrue),
        )
        .get_matches();

    debug!("UDPtoHLS: Command-line arguments parsed: {:?}", matches);

    println!("UDPtoHLS: version: {}", get_version());

    let drop_corrupt_ts = matches.get_flag("drop_corrupt_ts");
    let quiet = matches.get_flag("quiet");
    let pcap_stats_interval: u64 = matches
        .get_one::<String>("pcap_stats_interval")
        .unwrap()
        .parse()
        .unwrap_or(30);
    let endpoint = matches.get_one::<String>("endpoint").unwrap();
    let region_name = matches.get_one::<String>("region").unwrap();
    let bucket = matches.get_one::<String>("bucket").unwrap();
    let filter_ip = matches.get_one::<String>("udp_ip").unwrap();
    let filter_port: u16 = matches.get_one::<String>("udp_port").unwrap().parse()?;
    let interface = matches.get_one::<String>("interface").unwrap();
    let timeout: i32 = matches.get_one::<String>("timeout").unwrap().parse()?;
    let output_dir = matches.get_one::<String>("output_dir").unwrap();
    let generate_unsigned_urls = matches.get_flag("unsigned_urls");
    let verbose = matches
        .get_one::<String>("verbose")
        .unwrap()
        .parse()
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
    println!("UDPtoHLS: : Logging initialized. Starting main()...");

    let hls_keep_segments: usize = matches
        .get_one::<String>("hls_keep_segments")
        .unwrap()
        .parse()
        .unwrap_or(3);

    let diskless_ring_size: usize = matches
        .get_one::<String>("diskless_ring_size")
        .unwrap()
        .parse()
        .unwrap_or(1);

    fs::create_dir_all(output_dir)?;

    println!(
        "UDPtoHLS: Initializing S3 client with endpoint: {}",
        endpoint
    );
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
    let hourly_index_creator = Arc::new(Mutex::new(hourly_index_creator));

    println!("UDPtoHLS: Using channel output path: {}", output_dir);

    println!(
        "UDPtoHLS: Attempting to join multicast {}:{} on interface={}",
        filter_ip, filter_port, interface
    );
    let _sock = match join_multicast_on_iface(filter_ip, filter_port, interface) {
        Ok(s) => {
            println!(
                 "UDPtoHLS: Successfully joined multicast group on interface={} filter_ip={} filter_port={}",
                 interface, filter_ip, filter_port
             );
            s
        }
        Err(e) => {
            eprintln!(
                "UDPtoHLS: Failed to join multicast group or find interface IP: {}",
                e
            );
            return Ok(());
        }
    };

    println!(
        "UDPtoHLS: Opening PCAP on interface={} with snaplen={}, buffer={}b, timeout={}",
        get_snaplen(),
        get_buffer_size(),
        interface,
        timeout
    );

    let cap = Capture::from_device(interface.as_str())?
        .promisc(false)
        .buffer_size(get_buffer_size())
        .snaplen(get_snaplen())
        .timeout(timeout)
        .immediate_mode(false)
        .open()?;

    let mut cap = cap
        .setnonblock()
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

    let filter_expr = format!("udp and host {} and port {}", filter_ip, filter_port);
    println!("UDPtoHLS: Setting pcap filter to '{}'", filter_expr);
    cap.filter(&filter_expr, true)?;

    println!(
        "UDPtoHLS: Capturing on '{}', listening for {}:{}, writing HLS to '{}'",
        interface, filter_ip, filter_port, output_dir
    );

    // Build our ManualSegmenter
    let mut manual_segmenter = Some(
        ManualSegmenter::new(output_dir)
            .with_max_segments(hls_keep_segments)
            .with_diskless_ring_size(diskless_ring_size)
            .with_s3(
                Some(s3_client.clone()),
                Some(bucket.clone()),
                generate_unsigned_urls,
                Some(endpoint.clone()),
            )
            .with_diskless_max_bytes(get_max_segment_size_bytes())
            .with_hourly_index_creator(Some(hourly_index_creator.clone())),
    );

    // Diskless consumer thread (if needed)
    let diskless_consumer = {
        if let Some(_seg_ref) = &manual_segmenter {
            let buffer_ref = _seg_ref.diskless_buffer.clone();
            let shutdown_flag_clone = Arc::clone(&shutdown_flag);
            let handle = thread::spawn(move || loop {
                {
                    if shutdown_flag_clone.load(Ordering::SeqCst) {
                        break;
                    }
                    let mut buf = futures::executor::block_on(buffer_ref.lock());
                    if let Some(front) = buf.queue.pop_front() {
                        debug!(
                            "UDPtoHLS: Diskless consumer got a segment of len {}, dur={}",
                            front.data.len(),
                            front.duration
                        );
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            });
            Some(handle)
        } else {
            None
        }
    };

    let (pcap_tx, pcap_rx) = std_mpsc::sync_channel::<(Vec<u8>, u64)>(100000);

    let capture_shutdown = Arc::clone(&shutdown_flag);

    // Spawn a dedicated thread for the capture loop using its own runtime ---
    let capture_thread = std::thread::spawn(move || {
        // Create a mini–runtime for the capture thread
        let rt = tokio::runtime::Runtime::new().expect("Failed to create capture runtime");
        rt.block_on(async move {
              let mut stream = cap.stream(BoxCodec).unwrap();
              let mut stats_timer = Instant::now();
              let mut stats_last_recv = 0;
              let mut stats_last_drop = 0;
              while !capture_shutdown.load(Ordering::SeqCst) {
                  match stream.next().await {
                      Some(Ok((data, timestamp))) => {
                          let packet_data = Vec::from(&*data);
                          let timestamp_ms = timestamp
                              .duration_since(UNIX_EPOCH)
                              .map(|d| d.as_millis() as u64)
                              .unwrap_or_else(|_| {
                                  SystemTime::now()
                                      .duration_since(UNIX_EPOCH)
                                      .unwrap()
                                      .as_millis() as u64
                              });
                          if stats_timer.elapsed() >= Duration::from_secs(pcap_stats_interval) {
                              if let Ok(stats) = stream.capture_mut().stats() {
                                  let dropped = stats.dropped - stats_last_drop;
                                  let received = stats.received - stats_last_recv;
                                  if dropped > 0 || stats.if_dropped > 0 {
                                      log::error!(
                                          "UDPtoHLS: PCAP drops detected - Received: {}, Dropped: {}, Interface Dropped: {}",
                                          received, dropped, stats.if_dropped
                                      );
                                  }
                                  stats_last_recv = stats.received;
                                  stats_last_drop = stats.dropped;
                              }
                              stats_timer = Instant::now();
                          }
                          if let Err(e) = pcap_tx.send((packet_data, timestamp_ms)) {
                              log::error!("UDPtoHLS: Pcap Failed to send packet to channel: {:?}", e);
                              break;
                          }
                      }
                      None => {
                          tokio::time::sleep(Duration::from_millis(1)).await;
                      }
                      Some(Err(e)) => {
                          if e == pcap::Error::TimeoutExpired {
                              tokio::time::sleep(Duration::from_millis(1)).await;
                              continue;
                          }
                          log::error!("UDPtoHLS: Pcap error: {:?}", e);
                          break;
                      }
                  }
              }
              if let Ok(stats) = stream.capture_mut().stats() {
                  log::info!(
                      "UDPtoHLS: Final PCAP stats - Received: {}, Dropped: {}, Interface Dropped: {}",
                      stats.received,
                      stats.dropped,
                      stats.if_dropped
                  );
              }
          });
    });

    let mut pid_tracker = PidTracker::new();
    let mut leftover_ts = VecDeque::new();
    // Use a vector to accumulate TS packets as Arc slices to avoid unnecessary copies.
    let mut batch_buffer_arcs: Vec<Arc<[u8]>> = Vec::new();

    debug!("Starting main processing loop now...");
    loop {
        if shutdown_flag.load(Ordering::SeqCst) {
            println!("UDPtoHLS: Shutdown flag set, exiting main loop");
            break;
        }

        let (packet_data, timestamp) = match pcap_rx.recv() {
            Ok((data, ts)) => (data, ts),
            Err(e) => {
                log::error!("UDPtoHLS: Channel receive error: {:?}", e);
                if let Some(seg) = manual_segmenter.as_mut() {
                    if let Err(e) = seg.close_current_segment_file().await {
                        log::error!("UDPtoHLS: Error closing segment: {:?}", e);
                    }
                }
                break;
            }
        };

        if let Some(ts_data) = extract_mpegts_payload(&packet_data, filter_ip, filter_port) {
            leftover_ts.extend(ts_data.iter().copied());

            while leftover_ts.len() >= TS_PACKET_SIZE {
                let contiguous = leftover_ts.make_contiguous();

                // Ensure the packet starts with sync byte 0x47
                if contiguous[0] != 0x47 {
                    if let Some(pos) = contiguous.iter().position(|&x| x == 0x47) {
                        // Discard bytes until the next sync byte
                        log::warn!("UDPtoHLS: Invalid TS packet detected, discarding {} bytes of {} bytes total", pos, contiguous.len());
                        for _ in 0..pos {
                            leftover_ts.pop_front();
                        }
                    } else {
                        // No sync byte found, clear the buffer
                        log::error!(
                            "UDPtoHLS: Invalid TS packet detected, discarding all {} bytes",
                            contiguous.len()
                        );
                        leftover_ts.clear();
                        break;
                    }
                    continue;
                }

                if contiguous.len() < TS_PACKET_SIZE {
                    log::warn!("UDPtoHLS: Incomplete TS packet detected of {} bytes, waiting for more data", contiguous.len());
                    break;
                }

                // Create an Arc for the 188-byte TS packet without extra copying
                let packet_arc: Arc<[u8]> = Arc::from(&contiguous[..TS_PACKET_SIZE]);

                if drop_corrupt_ts {
                    // Process the packet (PID tracking)
                    match pid_tracker
                        .process_packet("ExtractMpegTSpayload".to_string(), packet_arc.as_ref())
                    {
                        Ok(()) => {}
                        Err(0xFFFF) => {
                            log::warn!("UDPtoHLS: Invalid TS packet detected (pid=0xFFFF), discarding and searching for next sync byte");
                            leftover_ts.pop_front();
                            continue;
                        }
                        Err(e) => {
                            log::error!("UDPtoHLS: Continuity error: {:?}", e);
                        }
                    }
                }

                // Accumulate valid packet into the batch buffer
                batch_buffer_arcs.push(packet_arc);

                // Remove the processed packet from the leftover buffer
                for _ in 0..TS_PACKET_SIZE {
                    leftover_ts.pop_front();
                }

                // Write in batches of 7 packets
                if batch_buffer_arcs.len() >= 7 {
                    let mut concatenated =
                        Vec::with_capacity(batch_buffer_arcs.len() * TS_PACKET_SIZE);
                    for arc in &batch_buffer_arcs {
                        concatenated.extend_from_slice(arc.as_ref());
                    }
                    if let Some(seg) = manual_segmenter.as_mut() {
                        if let Err(e) = seg.write_ts(timestamp, &concatenated).await {
                            log::error!("UDPtoHLS: Segment write error: {:?}", e);
                            break;
                        }
                    }
                    batch_buffer_arcs.clear();
                }
            }
        } else {
            log::warn!(
                "UDPtoHLS: Failed to extract MPEG-TS payload: 0x{:?}... {} bytes",
                hex::encode(&packet_data[..16]),
                packet_data.len()
            );
        }
    }

    shutdown_flag.store(true, Ordering::SeqCst);

    log::info!("UDPtoHLS: Waiting for capture thread to exit...");
    if let Err(e) = capture_thread.join() {
        log::error!("UDPtoHLS: Capture thread join error: {:?}", e);
    }

    if let Some(mut seg) = manual_segmenter.take() {
        log::info!("UDPtoHLS: Closing final segment...");
        if let Err(e) = seg.finalize().await {
            eprintln!("UDPtoHLS: Error closing final segment: {:?}", e);
        }
        drop(seg);
    }

    if let Some(handle) = diskless_consumer {
        log::info!("UDPtoHLS: Waiting for diskless consumer thread to exit...");
        if let Err(e) = handle.join() {
            log::error!("UDPtoHLS: Diskless consumer thread join error: {:?}", e);
        }
    }

    log::info!("UDPtoHLS: All threads/tasks exited. Shutting down.");
    Ok(())
}

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

    if payload[0] == 0x80 && payload.len() > 12 && payload[12] == 0x47 {
        let ts_payload = &payload[12..];
        if !ts_payload.is_empty() {
            return Some(ts_payload);
        } else {
            return None;
        }
    } else if payload[0] == 0x47 {
        return Some(payload);
    } else {
        warn!(
            "UDPtoHLS: Unknown payload type (expected TS or RTP-TS). First byte=0x{:02x}, size={}",
            payload[0],
            payload.len()
        );
        return None;
    }
}
