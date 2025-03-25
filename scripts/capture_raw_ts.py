#!/usr/bin/env python3
"""
Download and concatenate TS segments from an m3u8 playlist.
The script:
  - Fetches an m3u8 file from a provided HTTP URL.
  - Parses out segment (.ts) URLs (handles relative URLs properly).
  - Downloads each segment to a temporary directory with robust error handling and retries.
  - Concatenates all segments together into one final output TS file.
Usage:
    python m3u8_concat.py <m3u8_url> <output_file> [--temp-dir /path/to/dir] [--retries 3] [--timeout 10]
"""

import argparse
import logging
import os
import sys
import tempfile
import urllib.parse

import requests


def download_m3u8(url):
    """Download the m3u8 playlist and return its lines."""
    response = requests.get(url)
    response.raise_for_status()
    return response.text.splitlines()


def resolve_url(base, segment_url):
    """Resolve a segment URL relative to the base URL if needed."""
    return urllib.parse.urljoin(base, segment_url)


def download_segment(url, dest_path, retries=3, timeout=10):
    """Download a TS segment with retry logic."""
    for attempt in range(1, retries + 1):
        try:
            logging.info(f"Downloading segment: {url} (attempt {attempt}/{retries})")
            response = requests.get(url, stream=True, timeout=timeout)
            response.raise_for_status()
            with open(dest_path, 'wb') as f:
                for chunk in response.iter_content(chunk_size=8192):
                    if chunk:
                        f.write(chunk)
            return
        except Exception as e:
            logging.error(f"Error downloading {url} on attempt {attempt}: {e}")
            if attempt == retries:
                raise e


def concatenate_segments(segment_files, output_file):
    """Concatenate a list of segment files into a single output file."""
    with open(output_file, 'wb') as outfile:
        for seg_file in segment_files:
            logging.info(f"Appending {seg_file} to {output_file}")
            with open(seg_file, 'rb') as infile:
                outfile.write(infile.read())


def main():
    parser = argparse.ArgumentParser(
        description="Download TS segments from an m3u8 file and concatenate them into one TS file."
    )
    parser.add_argument("m3u8_url", help="URL of the m3u8 playlist file")
    parser.add_argument("output", help="Output TS file path")
    parser.add_argument(
        "--temp-dir",
        default=None,
        help="Temporary directory to store TS segments (default uses system temp directory)"
    )
    parser.add_argument(
        "--retries",
        type=int,
        default=3,
        help="Number of retries for downloading each segment (default: 3)"
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=10,
        help="Timeout in seconds for HTTP requests (default: 10)"
    )
    args = parser.parse_args()

    # Set up logging
    logging.basicConfig(level=logging.INFO, format="%(asctime)s - %(levelname)s - %(message)s")

    logging.info(f"Fetching m3u8 playlist from {args.m3u8_url}")
    try:
        m3u8_lines = download_m3u8(args.m3u8_url)
    except Exception as e:
        logging.error(f"Failed to fetch m3u8 playlist: {e}")
        sys.exit(1)

    # Parse the m3u8 file to extract segment URLs (ignoring comments)
    base_url = args.m3u8_url
    segment_urls = []
    for line in m3u8_lines:
        line = line.strip()
        if line and not line.startswith("#"):
            full_url = resolve_url(base_url, line)
            segment_urls.append(full_url)
    if not segment_urls:
        logging.error("No TS segments found in the m3u8 file.")
        sys.exit(1)
    logging.info(f"Found {len(segment_urls)} segments.")

    # Determine the temporary directory for storing segments
    temp_dir = args.temp_dir if args.temp_dir else tempfile.mkdtemp(prefix="m3u8_segments_")
    logging.info(f"Using temporary directory: {temp_dir}")

    downloaded_files = []
    for idx, seg_url in enumerate(segment_urls):
        seg_filename = os.path.join(temp_dir, f"segment_{idx:05d}.ts")
        try:
            download_segment(seg_url, seg_filename, retries=args.retries, timeout=args.timeout)
            downloaded_files.append(seg_filename)
        except Exception as e:
            logging.error(f"Failed to download segment {seg_url}: {e}")
            sys.exit(1)

    logging.info("All segments downloaded successfully. Starting concatenation.")
    try:
        concatenate_segments(downloaded_files, args.output)
    except Exception as e:
        logging.error(f"Failed to concatenate segments: {e}")
        sys.exit(1)

    logging.info(f"Concatenation complete. Output file created: {args.output}")


if __name__ == "__main__":
    main()