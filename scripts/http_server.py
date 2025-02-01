#!/usr/bin/env python3

import os
from http.server import HTTPServer, SimpleHTTPRequestHandler
from socketserver import ThreadingMixIn

class ThreadingHTTPServer(ThreadingMixIn, HTTPServer):
    """Handle requests in a separate thread for each client."""
    daemon_threads = True

class HLSHandler(SimpleHTTPRequestHandler):
    """HTTP handler that serves .m3u8 and .ts with correct headers,
    plus handles Range requests for .ts segments."""

    def do_HEAD(self):
        """Handle HEAD requests (just send headers, no body)."""
        if self.path.endswith('.m3u8'):
            self.send_m3u8_headers()
        elif self.path.endswith('.ts'):
            self.send_ts_headers()
        else:
            super().do_HEAD()

    def do_GET(self):
        """Handle GET requests, including partial (Range) requests."""
        if self.path.endswith('.m3u8'):
            # Serve HLS playlist
            if not os.path.isfile(self.path.lstrip('/')):
                self.send_error(404, "File not found")
                return
            self.send_m3u8_headers()
            with open(self.path.lstrip('/'), 'rb') as f:
                self.wfile.write(f.read())

        elif self.path.endswith('.ts'):
            # Serve TS segments with Range support
            local_path = self.path.lstrip('/')
            if not os.path.isfile(local_path):
                self.send_error(404, "File not found")
                return
            file_size = os.path.getsize(local_path)

            range_header = self.headers.get('Range')
            if range_header:
                # Example Range: bytes=0-1023
                try:
                    bytes_str = range_header.replace('bytes=', '').strip()
                    start_str, end_str = bytes_str.split('-', 1)
                    start = int(start_str) if start_str else 0
                    end = int(end_str) if end_str else file_size - 1
                except:
                    # If there's any parse error, ignore the range
                    start, end = 0, file_size - 1

                if start > end or start >= file_size:
                    self.send_error(416, "Requested Range Not Satisfiable")
                    return

                end = min(end, file_size - 1)  # clamp
                chunk_size = (end - start) + 1

                self.send_response(206)  # Partial Content
                self.send_header('Content-Type', 'video/mp2t')
                self.send_header('Access-Control-Allow-Origin', '*')
                self.send_header('Cache-Control', 'public, max-age=31536000')
                self.send_header('Pragma', 'public')
                self.send_header('Expires', '31536000')
                self.send_header('Content-Length', str(chunk_size))
                self.send_header('Content-Range', f'bytes {start}-{end}/{file_size}')
                self.end_headers()

                # Write the partial data
                with open(local_path, 'rb') as f:
                    f.seek(start)
                    self.wfile.write(f.read(chunk_size))

            else:
                # No Range header -> send entire file
                self.send_ts_headers(local_path)
                with open(local_path, 'rb') as f:
                    self.wfile.write(f.read())

        else:
            # Default to parent class for other file types
            super().do_GET()

    def send_m3u8_headers(self):
        """Send headers for an .m3u8 playlist."""
        self.send_response(200)
        self.send_header('Content-Type', 'application/vnd.apple.mpegurl')
        self.send_header('Access-Control-Allow-Origin', '*')
        # Cache-Control headers here
        self.send_header('Cache-Control', 'no-cache, no-store, must-revalidate')
        self.send_header('Pragma', 'no-cache')
        self.send_header('Expires', '0')
        self.end_headers()

    def send_ts_headers(self, local_path=None):
        """Send headers for a .ts file (full content)."""
        self.send_response(200)
        self.send_header('Content-Type', 'video/mp2t')
        self.send_header('Access-Control-Allow-Origin', '*')
        # Cache-Control headers here, cache the files for a year
        self.send_header('Cache-Control', 'public, max-age=31536000')
        self.send_header('Pragma', 'public')
        self.send_header('Expires', '31536000')
        if local_path:
            file_size = os.path.getsize(local_path)
            self.send_header('Content-Length', str(file_size))
        self.end_headers()

def run_server(port=80):
    server_address = ('', port)
    httpd = ThreadingHTTPServer(server_address, HLSHandler)
    print(f"Serving HLS content on port {port} (threaded)...")
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass
    httpd.server_close()
    print("Server stopped.")

if __name__ == '__main__':
    run_server(80)
