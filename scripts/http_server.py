#!/usr/bin/env python3

from http.server import HTTPServer, SimpleHTTPRequestHandler

class HLSHandler(SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.path.endswith('.m3u8'):
            self.send_response(200)
            self.send_header('Content-Type', 'application/vnd.apple.mpegurl')
            self.send_header('Access-Control-Allow-Origin', '*')
            self.end_headers()
            with open(self.path[1:], 'rb') as f:
                self.wfile.write(f.read())
        elif self.path.endswith('.ts'):
            self.send_response(200)
            self.send_header('Content-Type', 'video/mp2t')
            self.send_header('Access-Control-Allow-Origin', '*')
            self.end_headers()
            with open(self.path[1:], 'rb') as f:
                self.wfile.write(f.read())
        else:
            super().do_GET()

server_address = ('', 3001)
httpd = HTTPServer(server_address, HLSHandler)
print('Serving HLS content on port 3001...')
httpd.serve_forever()
