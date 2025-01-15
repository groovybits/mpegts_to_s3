/* 
 * mpegts_to_s3
 *
 * This program captures MPEG-TS packets from a UDP multicast stream, writes them to a pipe
 * that FFmpeg reads from, and then uploads the resulting HLS segments to an S3 bucket.
 *   
 * Chris Kennedy 2025 Jan 15 PoC for MpegTS to S3 with Ramdisk rewrite in Rust of
 *      the concept from: https://github.com/danielsobrado/ffmpeg-s3/tree/main
 *
 */
use aws_sdk_s3::Client;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::config::Credentials;
use aws_types::region::Region;
use clap::{Arg, Command as ClapCommand};
use notify::{RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher, EventKind, Event};
use pcap::Capture;
use std::fs;
use std::io::{Write, BufWriter};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use tokio::time::{sleep, Duration};
use log::{info, warn, debug};

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime};

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

async fn ensure_bucket_exists(client: &Client, bucket: &str) -> Result<(), Box<dyn std::error::Error>> {
    match client.head_bucket().bucket(bucket).send().await {
        Ok(_) => {
            info!("Bucket {} exists", bucket);
            Ok(())
        },
        Err(_) => {
            info!("Creating bucket {}", bucket);
            client.create_bucket()
                .bucket(bucket)
                .send()
                .await?;
            info!("Successfully created bucket {}", bucket);
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // --------------------------------------------------------------------
    // 1) Parse CLI
    // --------------------------------------------------------------------
    let matches = ClapCommand::new("mpegts_to_s3")
        .version("1.0")
        .about("PCAP capture -> FFmpeg HLS -> Directory Watch -> S3 Upload")
        .arg(Arg::new("endpoint")
            .short('e')
            .long("endpoint")
            .default_value("http://127.0.0.1:9000")
            .help("S3-compatible endpoint")
            .required(false))
        .arg(Arg::new("region")
            .short('r')
            .long("region")
            .default_value("us-west-2")
            .help("S3 region")
            .required(false))
        .arg(Arg::new("bucket")
            .short('b')
            .long("bucket")
            .default_value("ltnhls")
            .help("S3 bucket name")
            .required(false))
        .arg(Arg::new("udp_ip")
            .short('i')
            .long("udp_ip")
            .default_value("227.1.1.102")
            .help("UDP multicast IP to filter")
            .required(false))
        .arg(Arg::new("udp_port")
            .short('p')
            .long("udp_port")
            .default_value("4102")
            .help("UDP port to filter")
            .required(false))
        .arg(Arg::new("interface")
            .short('n')
            .long("interface")
            .default_value("net1")
            .help("Network interface for pcap")
            .required(false))
        .arg(Arg::new("timeout")
            .short('t')
            .long("timeout")
            .default_value("1000")
            .help("Capture timeout in milliseconds")
            .required(false))
        .arg(Arg::new("output_dir")
            .short('o')
            .long("output_dir")
            .default_value("hls")
            .help("Local dir for FFmpeg HLS output (could be a RAM disk)")
            .required(false))
        .arg(Arg::new("remove_local")
            .long("remove_local")
            .help("Remove local .ts/.m3u8 after uploading to S3?")
            .action(clap::ArgAction::SetTrue)
            .required(false))
        .get_matches();

    let endpoint = matches.get_one::<String>("endpoint").unwrap().to_string();
    let region_name = matches.get_one::<String>("region").unwrap().to_string();
    let bucket = matches.get_one::<String>("bucket").unwrap().to_string();
    let filter_ip = matches.get_one::<String>("udp_ip").unwrap().to_string();
    let filter_port: u16 = matches.get_one::<String>("udp_port").unwrap().parse()?;
    let interface = matches.get_one::<String>("interface").unwrap();
    let timeout: i32 = matches.get_one::<String>("timeout").unwrap().parse()?;
    let output_dir = matches.get_one::<String>("output_dir").unwrap().to_string();
    let remove_local = matches.get_flag("remove_local");

    info!("MpegTS to S3: endpoint={}, region={}, bucket={}, udp_ip={}, udp_port={}, interface={}, timeout={}, output_dir={}, remove_local={}",
         endpoint, region_name, bucket, filter_ip, filter_port, interface, timeout, output_dir, remove_local);

    info!("Creating output directory: {}", output_dir);
    // create output_dir if doesn't exist
    fs::create_dir_all(&output_dir)?;

    // --------------------------------------------------------------------
    // 2) Initialize S3 Client
    // --------------------------------------------------------------------
    info!("Initializing S3 client with endpoint: {}", endpoint);
    // In your S3 client initialization:
    let creds = Credentials::new(
        "minioadmin",     // access key
        "minioadmin",     // secret key
        None,            // session token
        None,            // expiry
        "dummy"          // provider name
    );

    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(Region::new(region_name))
        .endpoint_url(&endpoint)
        .credentials_provider(creds)
        .load()
        .await;

    let s3_config = aws_sdk_s3::config::Builder::from(&config)
        .force_path_style(true)
        .build();
    
    let s3_client = Client::from_conf(s3_config);

    ensure_bucket_exists(&s3_client, &bucket).await?;

    // --------------------------------------------------------------------
    // 3) Directory Watcher
    // --------------------------------------------------------------------
    info!("Starting directory watcher on: {} current dir {}", output_dir, std::env::current_dir()?.display());
    let (watch_tx, watch_rx) = channel();
    let watch_dir = output_dir.clone();
    let watch_thread = thread::spawn(move || {
        if let Err(e) = watch_directory(&watch_dir, watch_tx) {
            eprintln!("Directory watcher error: {:?}", e);
        }
    });

    // --------------------------------------------------------------------
    // 4) Spawn FFmpeg
    //    - Use time-based segmentation only.
    //    - Use -bsf:a aac_adtstoasc to handle non-ADTS AAC.
    //    - Write segments to subfolders with expansions, but main .m3u8 to a fixed path.
    // --------------------------------------------------------------------
    // Example final layout:
    //   hls/
    //     index.m3u8  <-- main playlist
    //     2025/
    //       01/
    //         14/
    //           23/
    //             segment_20250114-234519_0000.ts
    //             segment_20250114-234529_0001.ts
    //             ...

    let hls_segment_filename = format!("{}/%Y/%m/%d/%H/segment_%Y%m%d-%H%M%S_%04d.ts", output_dir);
    let m3u8_output = format!("index.m3u8"); // no expansions in main playlist

    info!("Starting FFmpeg with output: {}", m3u8_output);

    // Start ffmpeg
    let mut ffmpeg_child = Command::new("ffmpeg")
        .arg("-i").arg("pipe:0")
        .arg("-c").arg("copy")
        .arg("-loglevel").arg("error")
        .arg("-y")
        .arg("-hide_banner")
        .arg("-nostats")
        // bitstream filter for AAC so we don't get "AAC bitstream not in ADTS" errors
        //.arg("-bsf:a").arg("aac_adtstoasc")
        .arg("-max_delay").arg("500000")
        //.arg("-muxrate").arg("20M")
        .arg("-f").arg("hls")
        .arg("-map").arg("0")
        .arg("-hls_time").arg("2")
        //.arg("-hls_flags").arg("split_by_time")
        .arg("-hls_segment_type").arg("mpegts")
        .arg("-hls_playlist_type").arg("event")
        .arg("-hls_list_size").arg("0")
        .arg("-strftime").arg("1")
        .arg("-strftime_mkdir").arg("1")
        .arg("-hls_segment_filename").arg(&hls_segment_filename)
        .arg(&m3u8_output)
        .stdin(Stdio::piped())
        //.stdout(Stdio::null())
        //.stderr(Stdio::inherit())
        .spawn()?;

    // *** Take ownership of stdin so we can call ffmpeg_child.try_wait() freely
    let child_stdin = ffmpeg_child.stdin.take()
        .ok_or("Failed to take FFmpeg stdin")?;
    let mut ffmpeg_stdin_buf = BufWriter::new(child_stdin);

    // --------------------------------------------------------------------
    // 5) Launch an async task to watch new/modified files -> upload S3
    // --------------------------------------------------------------------
    let s3_client_clone = s3_client.clone();
    let bucket_clone = bucket.clone();
    let output_dir_clone = output_dir.clone();
    let upload_task = tokio::spawn(async move {
        handle_file_events(watch_rx, s3_client_clone, bucket_clone, output_dir_clone, remove_local).await;
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

    println!("Capturing on '{}', listening for {}:{}, writing HLS to '{}'", 
             interface, filter_ip, filter_port, output_dir);

    loop {
        // (a) Check if ffmpeg exited
        if let Some(exit_status) = ffmpeg_child.try_wait()? {
            eprintln!("FFmpeg ended. Code: {:?}", exit_status.code());
            break;
        }

        // (b) Grab next pcap packet
        let packet = match cap.next_packet() {
            Ok(pkt) => pkt,
            Err(_) => {
                // Possibly a timeout or no more packets
                if let Some(exit_status) = ffmpeg_child.try_wait()? {
                    eprintln!("FFmpeg ended. Code: {:?}", exit_status.code());
                    break;
                }
                continue;
            }
        };

        // (c) Extract the actual TS payload
        if let Some(ts_payload) = extract_mpegts_payload(&packet.data, &filter_ip, filter_port) {
            // (d) Write to ffmpeg's stdin
            if let Err(e) = ffmpeg_stdin_buf.write_all(ts_payload) {
                eprintln!("Error feeding data to FFmpeg: {:?}", e);
                break;
            }
        }
    }

    // --------------------------------------------------------------------
    // 7) Done capturing. Flush & close ffmpeg stdin
    // --------------------------------------------------------------------
    let _ = ffmpeg_stdin_buf.flush();
    drop(ffmpeg_stdin_buf);

    // If ffmpeg is still running, wait on it
    let ffmpeg_status = ffmpeg_child.wait()?;
    if !ffmpeg_status.success() {
        eprintln!("FFmpeg exited with error code: {:?}", ffmpeg_status.code());
    } else {
        println!("FFmpeg finished successfully.");
    }

    // Wait for background upload thread
    let _ = upload_task.await?;
    // Wait for watcher thread
    let _ = watch_thread.join();

    println!("Exiting normally.");
    Ok(())
}

//
// ===================== Directory Watcher ========================
//

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
                // Send to main
                if tx.send(event).is_err() {
                    break;
                }
            },
            Ok(Err(e)) => eprintln!("Notify error: {:?}", e),
            Err(_) => break,
        }
    }
    Ok(())
}

//
// ================== File-Event -> S3-Upload =====================
//

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
                                // Wait a moment so FFmpeg is done writing
                                sleep(Duration::from_millis(300)).await;
                                
                                if let Ok(()) = upload_file_to_s3(&s3_client, &bucket, &base_dir, &path, remove_local).await {
                                    let mut tracker = tracker_clone.lock().unwrap();
                                    tracker.mark_uploaded(path_str);
                                }
                            } else {
                                debug!("Skipping file {}: already uploaded or too old", path_str);
                            }
                        }
                    }
                }
            },
            _ => {}
        }
    }
}

async fn upload_file_to_s3(
    s3_client: &Client,
    bucket: &str,
    base_dir: &str,
    path: &Path,
    remove_local: bool
) -> Result<(), Box<dyn std::error::Error>> {
    let relative_path = strip_base_dir(path, base_dir)?;
    let key_str = relative_path.to_string_lossy().to_string();

    println!("Uploading {} -> s3://{}/{}", path.display(), bucket, key_str);

    // Add retry logic for potential transient failures
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
            },
            Err(e) => {
                retries -= 1;
                if retries == 0 {
                    return Err(Box::new(e));
                }
                eprintln!("Upload failed, retrying ({} attempts left): {:?}", retries, e);
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
    Ok(())
}

/// Strips the `base_dir` prefix from `full_path`.  
/// e.g. if `full_path = hls/2025/01/14/23/segment_20250114-234519_0000.ts`  
/// and `base_dir = hls`, returns `2025/01/14/23/segment_20250114-234519_0000.ts`.
fn strip_base_dir<'a>(full_path: &'a Path, base_dir: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let base = Path::new(base_dir).canonicalize()?;
    let full = full_path.canonicalize()?;
    let relative = full.strip_prefix(base)?;
    Ok(relative.to_path_buf())
}

//
// =================== PCAP -> TS Payload Parser ==================
//

fn extract_mpegts_payload<'a>(data: &'a [u8], filter_ip: &str, filter_port: u16) -> Option<&'a [u8]> {
    if data.len() < 14 { return None; }
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

    let udp_dst_port = u16::from_be_bytes([
        data[udp_header_offset + 2],
        data[udp_header_offset + 3],
    ]);
    let udp_length = u16::from_be_bytes([
        data[udp_header_offset + 4],
        data[udp_header_offset + 5],
    ]) as usize;

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

    // Determine if it's RTP and get the TS payload
    let (ts_payload, is_rtp) = if payload[0] == 0x80 && payload.len() > 12 && payload[12] == 0x47 {
        (&payload[12..], true)
    } else if payload[0] == 0x47 {
        (payload, false)
    } else {
        warn!("Unknown payload type found at offset {} of type {} size {}", udp_payload_offset, payload[0], payload.len());
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

    // Calculate number of complete TS packets
    let num_packets = ts_payload.len() / 188;
    let aligned_len = num_packets * 188;

    // Verify sync bytes for each TS packet
    for i in 0..num_packets {
        if ts_payload[i * 188] != 0x47 {
            warn!("Misaligned TS packet found at offset {}", i * 188);
            return None; // Misaligned packet found
        }
    }

    Some(&ts_payload[..aligned_len])
}
