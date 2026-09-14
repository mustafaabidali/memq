#!/usr/bin/env python3
"""Loopback Git HTTP fixture with a generated, test-only credential."""
import base64
import hmac
import http.server
import os
import subprocess
import sys
import urllib.parse


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def request(self):
        expected = "Basic " + base64.b64encode(
            ("fixture:" + os.environ["MEMQ_FIXTURE_CREDENTIAL"]).encode()
        ).decode()
        if not hmac.compare_digest(self.headers.get("Authorization", ""), expected):
            self.send_response(401)
            self.send_header("WWW-Authenticate", 'Basic realm="synthetic Git fixture"')
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        url = urllib.parse.urlsplit(self.path)
        if not url.path.startswith("/remote.git/"):
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", 0))
        if length > 16 * 1024 * 1024:
            self.send_error(413)
            return
        env = dict(os.environ, GIT_PROJECT_ROOT=sys.argv[1], GIT_HTTP_EXPORT_ALL="1",
                   PATH_INFO=url.path, QUERY_STRING=url.query,
                   REQUEST_METHOD=self.command, CONTENT_TYPE=self.headers.get("Content-Type", ""),
                   CONTENT_LENGTH=str(length), REMOTE_USER="fixture")
        backend = subprocess.run(["git", "http-backend"], input=self.rfile.read(length),
                                 env=env, capture_output=True, check=True, timeout=10)
        header, separator, body = backend.stdout.partition(b"\r\n\r\n")
        if not separator:
            raise RuntimeError("Git CGI header missing")
        fields = [line.split(b":", 1) for line in header.split(b"\r\n") if b":" in line]
        status = next((int(v.strip().split()[0]) for k, v in fields if k.lower() == b"status"), 200)
        self.send_response(status)
        for key, value in fields:
            if key.lower() not in (b"status", b"content-length"):
                self.send_header(key.decode(), value.strip().decode())
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    do_GET = request
    do_POST = request


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print(server.server_port, flush=True)
server.serve_forever()
