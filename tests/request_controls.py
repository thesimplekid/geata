#!/usr/bin/env python3
"""Regression checks for request resource controls on local test listeners."""
import argparse
import hashlib
import json
import subprocess
import http.client
import http.server
from pathlib import Path
import socket
import sys
import threading
import time

sys.dont_write_bytecode = True
from support import Processes, eventually, port, request, temporary_directory, tls_fixture


class Backend(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/stream":
            self.send_response(200)
            self.send_header("Content-Length", "20")
            self.end_headers()
            try:
                for _ in range(20):
                    self.wfile.write(b"x")
                    self.wfile.flush()
                    time.sleep(0.1)
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        if self.path == "/wait":
            time.sleep(2)
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        try:
            self.wfile.write(b"ok")
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_POST(self):
        if self.path == "/upload-wait":
            self.server.upload_started.set()
        if self.headers.get("Transfer-Encoding") == "chunked":
            while True:
                line = self.rfile.readline()
                if not line:
                    return
                size = int(line.strip(), 16)
                if not size:
                    self.rfile.readline()
                    break
                if len(self.rfile.read(size + 2)) != size + 2:
                    return
        else:
            size = int(self.headers.get("Content-Length", 0))
            if len(self.rfile.read(size)) != size:
                return
        self.do_GET()

    def log_message(self, *_):
        pass


def run(binary, root, group):
    backend = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Backend)
    backend.upload_started = threading.Event()
    threading.Thread(target=backend.serve_forever, daemon=True).start()
    hp, tp = port(), port()
    domain = "bounded.example.test"
    cert, key, ca = tls_fixture(root, domain)
    directory = f"https://localhost:{port()}/dir"
    data = root / "state"
    saved = data / hashlib.sha256(directory.encode()).hexdigest()
    saved.mkdir(parents=True, mode=0o700)
    data.chmod(0o700)
    pem = saved / f"{domain}.json"
    pem.write_text(json.dumps({"chain": cert.read_text(), "private_key": key.read_text()}))
    pem.chmod(0o600)
    config = root / "Geatafile"
    config.write_text(f"""http://ready.local {{ respond ready }}
http://bounded.local {{
    max_body_bytes 4
    request_timeout 1
    reverse_proxy 127.0.0.1:{backend.server_port}
}}
http://capacity.local {{
    max_inflight 1
    request_timeout 1
    reverse_proxy 127.0.0.1:{backend.server_port}
}}
{domain} {{
    request_timeout 1
    max_body_bytes 4
    reverse_proxy 127.0.0.1:{backend.server_port}
}}
http://unbounded.local {{
    request_timeout off
    max_body_bytes off
    reverse_proxy 127.0.0.1:{backend.server_port}
}}
http://paid.local {{
    pay 2 sat https://mint.example.com
    rate_limit 1/s burst 1
    payment_verify_limit 1/s burst 1
    respond paid
}}
""")
    group.start([binary, "run", "--config", str(config), "--data-dir", str(root / "state"),
                 "--http-listen", f"127.0.0.1:{hp}", "--https-listen", f"127.0.0.1:{tp}", "--acme-directory", directory], "controls")
    try:
        eventually(lambda: request(hp, "ready.local")[0] == 200)
        assert request(hp, "bounded.local", method="POST", body=b"1234")[0] == 200
        assert request(hp, "bounded.local", method="POST", body=b"12345")[0] == 413
        assert request(hp, "unbounded.local", method="POST", body=b"12345")[0] == 200
        connection = http.client.HTTPConnection("127.0.0.1", hp, timeout=4)
        connection.request("POST", "/", body=iter([b"123", b"45"]),
                           headers={"Host": "bounded.local"}, encode_chunked=True)
        assert connection.getresponse().status == 413
        connection.close()
        # An idle upload expires without needing a subsequent body callback.
        with socket.create_connection(("127.0.0.1", hp), timeout=4) as sock:
            sock.sendall(b"POST /upload-wait HTTP/1.1\r\nHost: capacity.local\r\nContent-Length: 4\r\n\r\n")
            assert backend.upload_started.wait(0.8), "upload was not admitted"
            assert request(hp, "capacity.local")[0] == 503
            started = time.monotonic()
            assert sock.recv(1024) == b""
            assert time.monotonic() - started < 2
        assert request(hp, "capacity.local")[0] == 200, "timeout leaked capacity"
        # Response waits use the same deadline, while an explicit opt-out works.
        try:
            request(hp, "bounded.local", "/wait")
            raise AssertionError("response exceeded deadline")
        except (http.client.RemoteDisconnected, ConnectionResetError):
            pass
        assert request(hp, "unbounded.local", "/wait")[0] == 200
        # Continuous response progress must not reset the total deadline.
        started = time.monotonic()
        try:
            request(hp, "bounded.local", "/stream")
            raise AssertionError("stream exceeded deadline")
        except http.client.IncompleteRead:
            pass
        assert time.monotonic() - started < 2
        curl = ["curl", "--silent", "--show-error", "--max-time", "4", "--noproxy", "*",
                "--http2", "--cacert", str(ca), "--resolve", f"{domain}:{tp}:127.0.0.1"]
        result = subprocess.run(curl + ["--write-out", "|%{http_version}", f"https://{domain}:{tp}/"],
                                capture_output=True, text=True, check=True)
        assert result.stdout == "ok|2", result
        started = time.monotonic()
        result = subprocess.run(curl + [f"https://{domain}:{tp}/wait"], capture_output=True)
        assert result.returncode != 0 and result.returncode != 28, result
        assert time.monotonic() - started < 2, "HTTP/2 stream did not expire"
        # The HTTP/2 body limit is applied before forwarding.
        result = subprocess.run(curl + ["--data-binary", "12345", "--write-out", "|%{http_code}",
                                       f"https://{domain}:{tp}/"], capture_output=True, text=True, check=True)
        assert result.stdout.endswith("|413"), result
        assert request(hp, "paid.local", headers={"X-Cashu": "invalid"})[0] == 400
        status, headers, _ = request(hp, "paid.local", headers={"X-Cashu": "invalid"})
        assert status == 429 and int(headers["Retry-After"]) >= 1
        assert request(hp, "paid.local")[0] == 200, "payment attempts consumed free allowance"
        # Rejected requests omit attacker-controlled host/path text from logs.
        assert request(hp, "unknown.local", "/rejection-log-marker")[0] == 404
        time.sleep(0.1)
        assert "rejection-log-marker" not in (root / "controls.log").read_text()
    finally:
        backend.shutdown()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    with temporary_directory("geata-controls-") as temporary, Processes(Path(temporary)) as group:
        run(str(args.binary.resolve()), Path(temporary), group)
    print("request control checks passed")
