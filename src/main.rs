/*
 * mpegts_to_s3
 *
 * This program captures MPEG-TS packets from a UDP multicast stream, either:
 *   A) Feeds them into FFmpeg (now via ffmpeg-next) which segments into HLS .ts/.m3u8, OR
 *   B) Manually segments the MPEG-TS and writes an .m3u8 playlist.
 *
 * Then, a directory watcher picks up new/modified files and uploads them to an S3 bucket.
 *
 * Chris Kennedy 2025 Jan 15
 */

extern crate ffmpeg_next as ffmpeg;

use std::sync::mpsc::Receiver;
use std::thread;

use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use aws_types::region::Region;
use clap::{Arg, Command as ClapCommand};
use ffmpeg_sys_next as ffmpeg_sys;
use get_if_addrs::get_if_addrs;
use log::{debug, error, info, warn};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use pcap::Capture;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::{BufWriter, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::{mpsc as std_mpsc, Arc};
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
            &format!("{}/index.m3u8", hour_dir),
            "application/vnd.apple.mpegurl",
        )
        .await?;

        let final_index_url = self
            .presign_get_url(&format!("{}/index.m3u8", hour_dir))
            .await?;
        self.rewrite_urls_log(hour_dir, &final_index_url)?;
        Ok(())
    }

    fn rewrite_urls_log(&mut self, hour_dir: &str, final_url: &str) -> std::io::Result<()> {
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

// ------------- MULTICAST JOIN + FILETRACKER + BUCKET CHECK -------------
// (unchanged)...

// ------------- DISKLESS MODE SUPPORT -------------
// (unchanged)...

// ------------- MANUAL SEGMENTER -------------
// (unchanged)...

// --------------------------------------------------------------------
// CREATE A FIFO, THEN let ffmpeg read from it
// --------------------------------------------------------------------
fn create_fifo_pipe(pipe_path: &str) -> std::io::Result<()> {
    let _ = fs::remove_file(pipe_path); // remove old file if any
    let status = Command::new("mkfifo").arg(pipe_path).status()?;
    if !status.success() {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to create FIFO: {}", pipe_path),
        ))
    } else {
        Ok(())
    }
}

// --------------------------------------------------------------------
// Minimal decode->encode pipeline injection
// --------------------------------------------------------------------
use ffmpeg::Packet as FfPacket;
use ffmpeg::Rational as FfRational;
use ffmpeg::{codec, format, media};
use ffmpeg_sys::{
    av_frame_alloc, av_frame_free, av_opt_set, av_packet_alloc, av_packet_unref,
    avcodec_alloc_context3, avcodec_find_decoder, avcodec_find_encoder, avcodec_open2,
    avcodec_parameters_from_context, avcodec_parameters_to_context, avcodec_receive_frame,
    avcodec_receive_packet, avcodec_send_frame, avcodec_send_packet, AVCodecContext, AVCodecID,
    AVPixelFormat,
};

/// Minimal: open a video decoder
unsafe fn open_video_decoder(in_par: *const ffmpeg_sys::AVCodecParameters) -> *mut AVCodecContext {
    let dec = avcodec_find_decoder((*in_par).codec_id);
    if dec.is_null() {
        warn!("No video decoder found => fallback to copy?");
        return std::ptr::null_mut();
    }
    let dec_ctx = avcodec_alloc_context3(dec);
    if dec_ctx.is_null() {
        warn!("alloc dec ctx fail");
        return std::ptr::null_mut();
    }
    let r = avcodec_parameters_to_context(dec_ctx, in_par);
    if r < 0 {
        warn!("avcodec_parameters_to_context => {}", r);
    }
    let r2 = avcodec_open2(dec_ctx, dec, std::ptr::null_mut());
    if r2 < 0 {
        warn!("avcodec_open2(dec) => {}", r2);
    }
    dec_ctx
}
/// Minimal: open H.264 encoder
unsafe fn open_video_encoder() -> *mut AVCodecContext {
    let enc = avcodec_find_encoder(AVCodecID::AV_CODEC_ID_H264);
    if enc.is_null() {
        warn!("No h264 encoder found => fallback to copy?");
        return std::ptr::null_mut();
    }
    let enc_ctx = avcodec_alloc_context3(enc);
    if enc_ctx.is_null() {
        warn!("alloc enc ctx fail");
        return std::ptr::null_mut();
    }
    // set resolution, AR, bitrate
    (*enc_ctx).width = 1280;
    (*enc_ctx).height = 720;
    (*enc_ctx).pix_fmt = AVPixelFormat::AV_PIX_FMT_YUV420P;
    (*enc_ctx).time_base.num = 1;
    (*enc_ctx).time_base.den = 30; // e.g. 30 fps
    (*enc_ctx).sample_aspect_ratio.num = 16;
    (*enc_ctx).sample_aspect_ratio.den = 9;
    (*enc_ctx).bit_rate = 2_000_000;

    let mut opts = std::ptr::null_mut();
    let r = avcodec_open2(enc_ctx, enc, &mut opts);
    if r < 0 {
        warn!("avcodec_open2(enc) => {}", r);
    }
    enc_ctx
}

/// feed TS packet -> decode -> re-encode -> write to container
unsafe fn decode_encode_video_packet(
    dec_ctx: *mut AVCodecContext,
    enc_ctx: *mut AVCodecContext,
    packet: &mut FfPacket,
    octx: &mut format::context::Output,
    out_idx: i32,
    tb_in: ffmpeg::Rational,
    tb_out: ffmpeg::Rational,
) {
    let mut avpkt = packet.as_av_packet_mut();
    avpkt.pts = packet.pts().unwrap_or(0) as i64;
    avpkt.dts = packet.dts().unwrap_or(0) as i64;
    let r = avcodec_send_packet(dec_ctx, &mut *avpkt);
    if r < 0 {
        warn!("avcodec_send_packet => {}", r);
    }
    av_packet_unref(&mut *avpkt);

    let mut frame = av_frame_alloc();
    while avcodec_receive_frame(dec_ctx, frame) == 0 {
        let r2 = avcodec_send_frame(enc_ctx, frame);
        if r2 < 0 {
            warn!("avcodec_send_frame => {}", r2);
        }
        let mut enc_pkt = av_packet_alloc();
        while avcodec_receive_packet(enc_ctx, enc_pkt) == 0 {
            let mut out_pkt = ffmpeg::Packet::empty();
            out_pkt.set_stream(out_idx);
            out_pkt.set_pts((*enc_pkt).pts as i64);
            out_pkt.set_dts((*enc_pkt).dts as i64);
            out_pkt.set_position(-1);
            let data_slice = std::slice::from_raw_parts((*enc_pkt).data, (*enc_pkt).size as usize);
            out_pkt.set_data(data_slice.to_vec());
            out_pkt.rescale_ts(tb_in, tb_out);
            out_pkt.write_interleaved(octx).ok();
            av_packet_unref(enc_pkt);
        }
        av_packet_unref(enc_pkt);
        ffmpeg_sys::av_packet_free(&mut enc_pkt);
    }
    av_frame_free(&mut frame);
}

/// Our "native_hls_segmenter" uses the named FIFO => TS data => HLS
/// With minimal decode->encode logic for video if `encode=true`.
fn native_hls_segmenter(
    fifo_path: &str,
    m3u8_output: &str,
    segment_filename: &str,
    encode: bool,
    hls_keep_segments: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    ffmpeg::init()?;

    let mut ictx = format::input(fifo_path)?;

    let dict = ffmpeg::Dictionary::new();
    let mut octx = format::output_with(m3u8_output, dict)?;

    // set HLS muxer options (unchanged)
    unsafe {
        let fmt_ctx = octx.as_mut_ptr();
        if !(*fmt_ctx).priv_data.is_null() {
            ffmpeg_sys::av_opt_set(
                (*fmt_ctx).priv_data,
                "hls_time\0".as_ptr() as *const i8,
                get_segment_duration_seconds().to_string().as_ptr() as *const i8,
                0,
            );
            ffmpeg_sys::av_opt_set(
                (*fmt_ctx).priv_data,
                "hls_segment_type\0".as_ptr() as *const i8,
                "mpegts\0".as_ptr() as *const i8,
                0,
            );
            ffmpeg_sys::av_opt_set(
                (*fmt_ctx).priv_data,
                "hls_playlist_type\0".as_ptr() as *const i8,
                "event\0".as_ptr() as *const i8,
                0,
            );
            ffmpeg_sys::av_opt_set(
                (*fmt_ctx).priv_data,
                "hls_list_size\0".as_ptr() as *const i8,
                hls_keep_segments.to_string().as_ptr() as *const i8,
                0,
            );
            ffmpeg_sys::av_opt_set(
                (*fmt_ctx).priv_data,
                "strftime\0".as_ptr() as *const i8,
                "1\0".as_ptr() as *const i8,
                0,
            );
            ffmpeg_sys::av_opt_set(
                (*fmt_ctx).priv_data,
                "strftime_mkdir\0".as_ptr() as *const i8,
                "1\0".as_ptr() as *const i8,
                0,
            );
            ffmpeg_sys::av_opt_set(
                (*fmt_ctx).priv_data,
                "hls_segment_filename\0".as_ptr() as *const i8,
                segment_filename.as_ptr() as *const i8,
                0,
            );
        }
    }

    // Build out streams
    let mut stream_mapping = vec![-1; ictx.nb_streams() as usize];
    let mut video_dec_ctx: *mut AVCodecContext = std::ptr::null_mut();
    let mut video_enc_ctx: *mut AVCodecContext = std::ptr::null_mut();
    let mut video_in_index = -1;
    let mut video_out_index = -1;
    let mut ost_index = 0;

    for (ist_index, ist) in ictx.streams().enumerate() {
        let medium = ist.parameters().medium();
        if medium != media::Type::Audio && medium != media::Type::Video {
            continue;
        }
        let mut ost = octx.add_stream(codec::Id::None)?;
        if !encode {
            // old style => just copy
            ost.set_parameters(ist.parameters());
            unsafe {
                (*ost.parameters().as_mut_ptr()).codec_tag = 0;
            }
        } else {
            // if video => do decode->encode
            if medium == media::Type::Video {
                video_in_index = ist_index as i32;
                unsafe {
                    video_dec_ctx = open_video_decoder(ist.parameters().as_ptr());
                    video_enc_ctx = open_video_encoder();
                    let r = avcodec_parameters_from_context(
                        ost.parameters().as_mut_ptr(),
                        video_enc_ctx,
                    );
                    if r < 0 {
                        warn!("avcodec_parameters_from_context => {}", r);
                    }
                    (*ost.parameters().as_mut_ptr()).codec_tag = 0;
                }
                video_out_index = ost_index;
            } else {
                // audio => we set it to AAC
                unsafe {
                    (*ost.parameters().as_mut_ptr()).codec_id = codec::Id::AAC.into();
                }
                ost.set_parameters(ist.parameters());
                unsafe {
                    (*ost.parameters().as_mut_ptr()).codec_tag = 0;
                }
            }
        }
        stream_mapping[ist_index] = ost_index as isize;
        ost_index += 1;
    }

    octx.write_header()?;

    let mut tb_in = vec![ffmpeg::Rational(0, 1); ictx.nb_streams() as usize];
    for (i, st) in ictx.streams().enumerate() {
        tb_in[i] = st.time_base();
    }

    // read packets
    for (stream, mut packet) in ictx.packets() {
        let ist_index = stream.index();
        let out_idx = stream_mapping[ist_index];
        if out_idx < 0 {
            continue;
        }
        let ost = octx
            .stream(out_idx as usize)
            .ok_or("missing output stream")?;
        if encode
            && ist_index as i32 == video_in_index
            && !video_dec_ctx.is_null()
            && !video_enc_ctx.is_null()
        {
            // decode->encode video
            unsafe {
                decode_encode_video_packet(
                    video_dec_ctx,
                    video_enc_ctx,
                    &mut packet,
                    &mut octx,
                    out_idx as i32,
                    tb_in[ist_index],
                    ost.time_base(),
                );
            }
        } else {
            // copy
            packet.rescale_ts(tb_in[ist_index], ost.time_base());
            packet.set_stream(out_idx as i32);
            packet.write_interleaved(&mut octx)?;
        }
    }

    // flush decode->encode if needed
    if encode && !video_dec_ctx.is_null() && !video_enc_ctx.is_null() {
        let mut flush_packet = ffmpeg::Packet::empty();
        unsafe {
            decode_encode_video_packet(
                video_dec_ctx,
                video_enc_ctx,
                &mut flush_packet,
                &mut octx,
                video_out_index,
                ffmpeg::Rational(1, 1),
                ffmpeg::Rational(1, 1),
            );
        }
    }

    octx.write_trailer()?;
    Ok(())
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
    let manual_segment = matches.get_flag("manual_segment");
    let generate_unsigned_urls = matches.get_flag("unsigned_urls");
    let encode = matches.get_flag("encode");
    let diskless_mode = matches.get_flag("diskless_mode");

    if manual_segment && encode {
        eprintln!("Cannot use --manual_segment and --encode together.");
        process::exit(1);
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
                    diskless_mode={}, diskless_ring_size={}, encode={}",
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
    let hourly_index_creator = Arc::new(Mutex::new(hourly_index_creator));

    info!("Starting directory watcher on: {}", output_dir);
    let (watch_tx, watch_rx) = std_mpsc::channel();
    let watch_dir = output_dir.to_string();
    let watch_thread = thread::spawn(move || {
        if let Err(e) = watch_directory(&watch_dir, watch_tx) {
            eprintln!("Directory watcher error: {:?}", e);
        }
    });

    // We'll define a channel for TS data that we feed to a background thread:
    let (ffmpeg_thread_handle, ffmpeg_data_sender) = if !manual_segment {
        let hls_segment_filename =
            format!("{}/%Y/%m/%d/%H/segment_%Y%m%d-%H%M%S_%04d.ts", output_dir);
        let m3u8_output = "index.m3u8".to_string();

        info!(
            "Starting native ffmpeg-next pipeline with output: {}",
            m3u8_output
        );

        // Create a named FIFO to feed TS data
        let fifo_path = format!("/tmp/hls_input_{}.ts", std::process::id());
        create_fifo_pipe(&fifo_path).expect("Failed to create named pipe at /tmp/hls_input.ts");

        // Next, we create the channel
        let (tx, rx) = std_mpsc::channel::<Vec<u8>>();

        // 1) Start a "FIFO writer" thread: read from rx, write to the FIFO
        let _fifo_writer_thread = std::thread::spawn({
            let pipe_clone = fifo_path.clone();
            move || {
                let mut fifo_file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&pipe_clone)
                    .expect("Could not open FIFO for writing");
                while let Ok(ts_chunk) = rx.recv() {
                    if let Err(e) = fifo_file.write_all(&ts_chunk) {
                        eprintln!("Error writing TS data to FIFO: {:?}", e);
                        break;
                    }
                }
                eprintln!("FIFO writer thread done => no more TS data.");
            }
        });

        let fifo_path_clone = fifo_path.clone();

        // 2) Start a second thread for the actual HLS muxer reading from that FIFO
        let hls_thread = std::thread::spawn({
            let hls_segfile = hls_segment_filename.clone();
            move || {
                if let Err(e) = native_hls_segmenter(
                    &fifo_path_clone,
                    &m3u8_output,
                    &hls_segfile,
                    encode,
                    hls_keep_segments,
                ) {
                    eprintln!("native_hls_segmenter error: {:?}", e);
                }
                eprintln!("native_hls_segmenter thread ending.");
            }
        });

        // We only store the "HLS thread" handle for minimal changes,
        // the writer thread we just let run on its own.
        (Some(hls_thread), Some(tx))
    } else {
        (None, None)
    };

    // --------------- UPLOAD TASK ---------------
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

    // If we are doing manual segmentation => build a segmenter
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

    // If diskless => we can run a separate consumer thread
    let diskless_consumer = if diskless_mode {
        if let Some(_seg_ref) = &manual_segmenter {
            let buffer_ref = _seg_ref.diskless_buffer.clone();
            let handle = thread::spawn(move || loop {
                {
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
        let packet = match cap.next_packet() {
            Ok(pkt) => pkt,
            Err(_) => {
                // Possibly no packet arrived in the last 'timeout' ms
                // If manual segment, close segment on inactivity:
                if let Some(seg) = manual_segmenter.as_mut() {
                    if let Err(e) = seg.close_current_segment_file().await {
                        eprintln!("Error closing segment: {:?}", e);
                        break;
                    }
                }
                continue;
            }
        };

        if let Some(ts_payload) = extract_mpegts_payload(&packet.data, filter_ip, filter_port) {
            // If using native ffmpeg pipeline
            if let Some(ref tx) = ffmpeg_data_sender {
                // feed TS data to the channel => the FIFO writer => the named pipe => FFmpeg
                if let Err(e) = tx.send(ts_payload.to_vec()) {
                    eprintln!("Error sending data to native ffmpeg thread: {:?}", e);
                    break;
                }
            }

            // If manual segmenting
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

    // We are done capturing -> If we had a channel open, close it so the thread can exit
    if let Some(tx) = ffmpeg_data_sender {
        drop(tx); // signal end-of-stream
    }

    // Join the native ffmpeg thread, if any
    if let Some(handle) = ffmpeg_thread_handle {
        let _ = handle.join();
    }

    drop(manual_segmenter);

    // If we had a diskless consumer thread, we can let it end
    if let Some(handle) = diskless_consumer {
        let _ = handle.thread().id();
    }

    let _ = upload_task.await?;
    let _ = watch_thread.join();
    println!("Exiting normally.");
    Ok(())
}

// ---------------- Directory Watcher ----------------

fn watch_directory(dir_path: &str, tx: std::sync::mpsc::Sender<Event>) -> notify::Result<()> {
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

                            // Wait for the file to stabilize
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
