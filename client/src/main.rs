use anyhow::{anyhow, Result};
use clap::{Arg, ArgAction, Command as ClapCommand};
use m3u8_rs::{parse_playlist_res, Playlist};
use reqwest::blocking::Client;
use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;
use url::Url;

/// A rolling record of processed segment URIs.
/// - We store URIs in a HashSet for quick "have we seen this URI?" lookups.
/// - We also store them in a queue (VecDeque) with a fixed capacity
///   so we don't keep everything forever.
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

    /// Check if we've already processed `uri`.
    fn contains(&self, uri: &str) -> bool {
        self.seen.contains(uri)
    }

    /// Mark `uri` as processed.
    fn insert(&mut self, uri: String) {
        // Insert into the set.
        self.seen.insert(uri.clone());
        // Push it onto the queue.
        self.queue.push_back(uri);

        // If we exceed capacity, pop from the front.
        while self.queue.len() > self.capacity {
            if let Some(removed) = self.queue.pop_front() {
                self.seen.remove(&removed);
            }
        }
    }
}

/// Resolve segment URLs correctly (handles relative paths).
fn resolve_segment_url(base_url: &Url, segment_path: &str) -> Result<Url> {
    base_url
        .join(segment_path)
        .map_err(|e| anyhow!("Failed to resolve segment URL: {}", e))
}

fn main() -> Result<()> {
    // ----------------------------------------------------------------
    // 1) Parse command-line arguments (Clap v4 syntax)
    // ----------------------------------------------------------------
    let matches = ClapCommand::new("hls-to-udp")
        .version("1.0")
        .about("Fetches HLS segments and pipes them to FFmpeg, outputting real-time MPEG-TS over UDP.")
        .arg(
            Arg::new("m3u8_url")
                .long("m3u8-url")
                .short('u')
                .action(ArgAction::Set)
                .required(true)
                .help("URL of the M3U8 playlist (HLS)."),
        )
        .arg(
            Arg::new("udp_output")
                .long("udp-output")
                .short('o')
                .action(ArgAction::Set)
                .required(true)
                .help("UDP output destination, e.g. udp://127.0.0.1:1234."),
        )
        .arg(
            Arg::new("history_size")
                .long("history-size")
                .short('s')
                .action(ArgAction::Set)
                .default_value("2000")
                .help("Number of unique segments to remember to avoid re-downloading (default: 2000)."),
        )
        .arg(
            Arg::new("interval")
                .long("interval")
                .short('i')
                .action(ArgAction::Set)
                .default_value("1")
                .help("Interval in seconds to check for new segments (default: 1)."),
        )
        .arg(
            Arg::new("muxrate")
                .long("muxrate")
                .short('r')
                .action(ArgAction::Set)
                .default_value("15M")
                .help("Muxrate for FFmpeg output (default: 15M)."),
        )
        .get_matches();

    // Get arguments as Strings:
    let m3u8_url = matches.get_one::<String>("m3u8_url").unwrap();
    let udp_output = matches.get_one::<String>("udp_output").unwrap();
    let interval = matches
        .get_one::<String>("interval")
        .unwrap()
        .parse::<u64>()
        .unwrap_or(1);
    let history_capacity = matches
        .get_one::<String>("history_size")
        .unwrap()
        .parse::<usize>()
        .unwrap_or(2000);

    println!("Starting HLS -> UDP with:");
    println!("  M3U8 URL       = {}", m3u8_url);
    println!("  UDP output     = {}", udp_output);
    println!("  History size   = {}", history_capacity);

    // Parse base URL for relative segment resolution
    let base_url = Url::parse(m3u8_url).map_err(|e| anyhow!("Could not parse M3U8 URL: {}", e))?;

    // ----------------------------------------------------------------
    // 2) Create our segment history (do not re-download if seen)
    // ----------------------------------------------------------------
    let mut segment_history = SegmentHistory::new(history_capacity);

    // ----------------------------------------------------------------
    // 3) Start FFmpeg as a child process with real-time options
    // ----------------------------------------------------------------
    let mut ffmpeg_child = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("info")
        // We'll feed an MPEG-TS stream from stdin:
        .arg("-f")
        .arg("mpegts")
        // -re can be used to throttle output in real-time:
        .arg("-re")
        .arg("-i")
        .arg("pipe:0")
        .arg("-muxrate")
        .arg(matches.get_one::<String>("muxrate").unwrap())
        // Copy codecs (no transcoding):
        .arg("-c")
        .arg("copy")
        // Output MPEG-TS over UDP:
        .arg("-f")
        .arg("mpegts")
        .arg("-fflags")
        .arg("+genpts")
        .arg(udp_output)
        .stdin(Stdio::piped())
        .spawn()?;

    let ffmpeg_stdin = ffmpeg_child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("Failed to capture FFmpeg stdin"))?;

    // ----------------------------------------------------------------
    // 4) Main loop to repeatedly fetch and parse the M3U8 playlist
    // ----------------------------------------------------------------
    let client = Client::new();

    loop {
        // ------------------------------------------------------------
        // 4a) Fetch the M3U8 playlist
        // ------------------------------------------------------------
        let playlist_data = match client.get(m3u8_url).send() {
            Ok(resp) => {
                if !resp.status().is_success() {
                    eprintln!(
                        "Error fetching playlist (status = {}): retrying...",
                        resp.status()
                    );
                    sleep(Duration::from_secs(5));
                    continue;
                }
                resp.text()?
            }
            Err(e) => {
                eprintln!("Failed to fetch playlist: {}. Retrying...", e);
                sleep(Duration::from_secs(5));
                continue;
            }
        };

        // ------------------------------------------------------------
        // 4b) Parse the M3U8 playlist (convert String -> &[u8])
        // ------------------------------------------------------------
        let parsed_playlist = parse_playlist_res(playlist_data.as_bytes());

        let media_playlist = match parsed_playlist {
            Ok(Playlist::MediaPlaylist(mp)) => mp,
            Ok(_) => {
                eprintln!("Parsed a non-Media playlist (master or unknown). Retrying...");
                sleep(Duration::from_secs(5));
                continue;
            }
            Err(e) => {
                eprintln!("Failed to parse M3U8 playlist: {:?}. Retrying...", e);
                sleep(Duration::from_secs(5));
                continue;
            }
        };

        // If `end_list=true`, it’s a VOD that’s presumably final.
        let is_vod = media_playlist.end_list;

        // ------------------------------------------------------------
        // 4c) Download each *new* segment in the playlist
        // ------------------------------------------------------------
        for segment in &media_playlist.segments {
            let uri = &segment.uri;
            // If we have processed this URI before, skip it.
            if segment_history.contains(uri) {
                // Already processed -> skip
                continue;
            }

            // Otherwise, resolve and download
            let segment_url = resolve_segment_url(&base_url, uri)?;
            println!("Downloading new segment: {}", segment_url);

            let segment_data = client.get(segment_url).send()?.bytes()?;

            // Feed segment to FFmpeg
            ffmpeg_stdin.write_all(&segment_data)?;
            ffmpeg_stdin.flush()?;

            // Remember this segment
            segment_history.insert(uri.to_string());
        }

        // ------------------------------------------------------------
        // 4d) If this is VOD with an `ENDLIST`, we can stop
        // ------------------------------------------------------------
        if is_vod {
            println!("VOD playlist has `#EXT-X-ENDLIST`; stopping.");
            break;
        }

        // ------------------------------------------------------------
        // 4e) Sleep before refetching the playlist N seconds
        // ------------------------------------------------------------
        sleep(Duration::from_secs(interval));
    }

    // ----------------------------------------------------------------
    // 5) Gracefully terminate FFmpeg
    // ----------------------------------------------------------------
    match ffmpeg_child.kill() {
        Ok(_) => println!("Stopped FFmpeg."),
        Err(e) => eprintln!("Failed to kill FFmpeg: {}", e),
    }

    Ok(())
}
