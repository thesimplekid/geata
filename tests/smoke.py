#!/usr/bin/env python3
"""Exercise the built binary over real sockets; optionally test ACME with Pebble."""

import argparse
import base64
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import signal
import ssl
import subprocess
import sys
import threading
import time

sys.dont_write_bytecode = True
import acme_faults
import regressions
from support import Processes, connect, eventually, port, request, stop, temporary_directory, tls_fixture


class Backend(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_GET(self):
        if self.headers.get("Upgrade", "").lower() == "websocket":
            key = self.headers["Sec-WebSocket-Key"]
            accept = base64.b64encode(hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()
            ).digest()).decode()
            self.send_response(101)
            self.send_header("Upgrade", "websocket")
            self.send_header("Connection", "Upgrade")
            self.send_header("Sec-WebSocket-Accept", accept)
            self.end_headers()
            frame = self.rfile.read(2)
            assert frame == b"\x81\x82"
            mask = self.rfile.read(4)
            payload = self.rfile.read(2)
            decoded = bytes(value ^ mask[i % 4] for i, value in enumerate(payload))
            self.wfile.write(b"\x81\x02" + decoded)
            self.wfile.flush()
            self.close_connection = True
            return
        self.reply()

    def do_POST(self):
        self.reply()

    def reply(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        content = json.dumps({
            "backend": self.server.label,
            "path": self.path,
            "body": body.decode(),
            "headers": {key.lower(): value for key, value in self.headers.items()},
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(content)))
        self.end_headers()
        if self.path == "/slow":
            self.wfile.write(content[:1])
            self.wfile.flush()
            time.sleep(4)
            self.wfile.write(content[1:])
        else:
            self.wfile.write(content)


def websocket(port_number, domain, context=None):
    with connect(port_number, domain, context) as sock:
        sock.sendall((f"GET /ws HTTP/1.1\r\nHost: {domain}\r\n"
                      "Connection: Upgrade\r\nUpgrade: websocket\r\n"
                      "Sec-WebSocket-Version: 13\r\n"
                      "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").encode())
        response = b""
        while not response.endswith(b"\r\n\r\n"):
            chunk = sock.recv(1)
            assert chunk, "connection closed during WebSocket upgrade"
            response += chunk
        assert response.startswith(b"HTTP/1.1 101"), response
        sock.sendall(b"\x81\x82\x01\x02\x03\x04" + bytes([ord("h") ^ 1, ord("i") ^ 2]))
        echoed = b""
        while len(echoed) < 4:
            chunk = sock.recv(4 - len(echoed))
            assert chunk
            echoed += chunk
        assert echoed == b"\x81\x02hi", echoed


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--pebble-bin", type=Path, help="directory containing pebble and pebble-challtestsrv")
    args = parser.parse_args()
    binary = str(args.binary.resolve())
    assert subprocess.check_output([binary, "--version"], text=True).startswith("geata ")
    assert "geata" in subprocess.check_output([binary, "--help"], text=True)
    backends = []
    with temporary_directory("geata-smoke-") as temporary, Processes(Path(temporary)) as processes:
        root = Path(temporary)
        start = processes.start
        regression_root = root / "regressions"
        regression_root.mkdir()
        with Processes(regression_root) as regression_group:
            regressions.run(binary, regression_root, regression_group)

        try:
            for label in ("A", "B"):
                server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Backend)
                server.label = label
                backends.append(server)
                threading.Thread(target=server.serve_forever, daemon=True).start()
            backend_a, backend_b = (server.server_port for server in backends)
            http_port, https_port = port(), port()
            config = root / "Geatafile"

            def write_config(text):
                replacement = root / "Geatafile.new"
                replacement.write_text(text)
                replacement.replace(config)

            write_config(f"http://localhost {{ reverse_proxy localhost:{backend_a} }}\n")
            subprocess.run([binary, "validate", "--config", str(config)], check=True)
            command = [binary, "run", "--config", str(config), "--data-dir", str(root / "state"),
                       "--http-listen", f"127.0.0.1:{http_port}", "--https-listen", f"127.0.0.1:{https_port}"]
            proxy = start(command, "proxy-http")
            eventually(lambda: request(http_port, "localhost")[0] == 200)
            status, _, content = request(http_port, "localhost", "/hello?a=1", method="POST", body=b"payload",
                                         headers={"X-Forwarded-For": "spoofed", "Forwarded": "for=spoofed", "X-Forwarded-Proto": "https"})
            data = json.loads(content)
            assert status == 200 and data["path"] == "/hello?a=1" and data["body"] == "payload"
            assert data["headers"]["x-forwarded-for"] == "127.0.0.1"
            assert data["headers"]["x-forwarded-proto"] == "http"
            assert "forwarded" not in data["headers"]
            assert request(http_port, "unknown.test")[0] == 404
            assert request(http_port, "localhost", "/.well-known/acme-challenge/missing")[0] == 404
            websocket(http_port, "localhost")

            # An in-flight response retains its original backend while new requests use the edit.
            slow = connect(http_port, "localhost")
            slow.sendall(b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            response = http.client.HTTPResponse(slow)
            response.begin()
            write_config(f"http://localhost {{ reverse_proxy 127.0.0.1:{backend_b} }}\n")
            eventually(lambda: json.loads(request(http_port, "localhost")[2])["backend"] == "B")
            assert json.loads(response.read())["backend"] == "A"
            slow.close()
            write_config("localhost { typo }")
            time.sleep(3)
            assert json.loads(request(http_port, "localhost")[2])["backend"] == "B"
            invalid = subprocess.run([binary, "validate", "--config", str(config)], capture_output=True)
            assert invalid.returncode != 0

            # Switch from a backend to a direct response, including literal syntax characters.
            literal = 'Hello # {world}! "quoted" café\n'
            write_config(f'http://localhost {{ respond {json.dumps(literal)} 201 }}\n')
            eventually(lambda: request(http_port, "localhost")[0] == 201)
            status, headers, content = request(http_port, "localhost", "/any/path")
            assert content == literal.encode() and headers["Content-Length"] == str(len(content))
            assert headers["Content-Type"] == "text/plain; charset=utf-8"
            status, headers, content = request(http_port, "localhost", method="HEAD")
            assert status == 201 and content == b"" and headers["Content-Length"] == str(len(literal.encode()))
            write_config('http://localhost { respond "broken }')
            time.sleep(3)
            assert request(http_port, "localhost")[2] == literal.encode()
            for code in (204, 205, 304):
                write_config(f"http://localhost {{ respond {code} }}")
                eventually(lambda: request(http_port, "localhost")[0] == code)
                status, headers, content = request(http_port, "localhost")
                assert content == b""
                if code != 205:
                    assert "Content-Length" not in headers
            write_config('http://localhost { respond "" }')
            eventually(lambda: request(http_port, "localhost")[0] == 200)
            assert request(http_port, "localhost")[2] == b""
            write_config(f"http://localhost {{ reverse_proxy 127.0.0.1:{backend_b} }}")
            eventually(lambda: request(http_port, "localhost")[2].startswith(b'{"backend"'))
            assert json.loads(request(http_port, "localhost")[2])["backend"] == "B"
            stop(proxy)
            print("PASS: routing, WebSockets, reloads, direct responses, quoting, HEAD, empty/status-only responses", flush=True)

            if not args.pebble_bin:
                return

            cert, key, ca_cert = tls_fixture(root)
            ca_port, management_port, dns_port, dns_management = port(), port(), port(), port()
            pebble_config = root / "pebble.json"
            pebble_config.write_text(json.dumps({"pebble": {
                "listenAddress": f"127.0.0.1:{ca_port}", "managementListenAddress": f"127.0.0.1:{management_port}",
                "certificate": str(cert), "privateKey": str(key), "httpPort": http_port, "tlsPort": https_port,
                "certificateValidityPeriod": 60,
            }}))
            start([str(args.pebble_bin / "pebble-challtestsrv"), "-dns01", f"127.0.0.1:{dns_port}",
                   "-management", f"127.0.0.1:{dns_management}", "-defaultIPv6", "", "-http01", "", "-https01", "",
                   "-tlsalpn01", "", "-doh", ""], "dns")
            ca = start([str(args.pebble_bin / "pebble"), "-config", str(pebble_config), "-dnsserver", f"127.0.0.1:{dns_port}"],
                       "pebble", env={**os.environ, "PEBBLE_VA_NOSLEEP": "1", "PEBBLE_WFE_NONCEREJECT": "0", "PEBBLE_AUTHZREUSE": "0"})
            bootstrap = ssl.create_default_context(cafile=str(ca_cert))
            eventually(lambda: request(ca_port, "localhost", "/dir", bootstrap)[0] == 200)
            root_pem = eventually(lambda: request(management_port, "localhost", "/roots/0", bootstrap)[2])
            trust = ssl.create_default_context(cadata=root_pem.decode())
            site_root = root / "site-root.pem"
            site_root.write_bytes(root_pem)
            domain = "app.example.test"
            write_config(f"{domain} {{ reverse_proxy 127.0.0.1:{backend_a} }}\n")
            command += ["--acme-directory", f"https://localhost:{ca_port}/dir", "--acme-root", str(ca_cert)]
            proxy = start(command, "proxy-acme")
            status, headers, _ = eventually(lambda: request(http_port, domain, "/hello?q=1"))
            assert status == 308 and headers["Location"] == f"https://{domain}:{https_port}/hello?q=1"
            eventually(lambda: request(https_port, domain, context=trust)[0] == 200, timeout=60)
            websocket(https_port, domain, trust)
            assert json.loads(request(https_port, domain, context=trust)[2])["headers"]["x-forwarded-proto"] == "https"
            http_version = subprocess.check_output([
                "curl", "--silent", "--show-error", "--fail", "--max-time", "10", "--noproxy", "*",
                "--http2", "--cacert", str(site_root), "--resolve", f"{domain}:{https_port}:127.0.0.1",
                "--output", os.devnull, "--write-out", "%{http_version}", f"https://{domain}:{https_port}/",
            ], text=True)
            assert http_version == "2", f"expected HTTP/2, got {http_version}"

            def fingerprint():
                with connect(https_port, domain, trust) as sock:
                    return hashlib.sha256(sock.getpeercert(binary_form=True)).hexdigest()

            first = fingerprint()
            account_files = list((root / "state").glob("*/account.json"))
            assert len(account_files) == 1
            credentials = account_files[0].read_bytes()
            assert account_files[0].stat().st_mode & 0o777 == 0o600
            print("PASS: real HTTP-01 validation, trusted TLS chain, HTTP/2, HTTPS redirect, secure credential storage", flush=True)
            renewed = eventually(lambda: (value if (value := fingerprint()) != first else None), timeout=75)
            print("PASS: short-lived certificate renewed and replaced without restarting", flush=True)

            # Multiple sites acquire independent certificates on a live reload.
            other = "other.example.test"
            write_config(f'{domain} {{ reverse_proxy 127.0.0.1:{backend_a} }}\n{other} {{ respond "Hello world!" }}\n')
            eventually(lambda: request(https_port, other, context=trust)[0] == 200, timeout=45)
            assert request(https_port, other, context=trust)[2] == b"Hello world!"
            assert request(http_port, other)[0] == 308
            direct_h2 = subprocess.check_output([
                "curl", "--silent", "--show-error", "--fail", "--max-time", "10", "--noproxy", "*",
                "--http2", "--cacert", str(site_root), "--resolve", f"{other}:{https_port}:127.0.0.1",
                "--write-out", "|%{http_version}", f"https://{other}:{https_port}/",
            ], text=True)
            assert direct_h2 == "Hello world!|2"
            write_config(f"{domain} {{ reverse_proxy 127.0.0.1:{backend_a} }}\n")
            eventually(lambda: request(http_port, other)[0] == 404)
            try:
                with connect(https_port, other, trust):
                    raise AssertionError("removed site's TLS handshake unexpectedly succeeded")
            except ssl.SSLError:
                pass
            # Failed HTTP-01 validation recovers after DNS is corrected, without restart.
            broken = "broken.example.test"
            request(dns_management, "localhost", "/add-a", method="POST",
                    body=json.dumps({"host": broken, "addresses": ["127.0.0.2"]}).encode())
            write_config(f'{domain} {{ respond "healthy" }}\n{broken} {{ respond "recovered" }}')
            journal = account_files[0].with_name("retries.json")
            def failed_validation():
                value = json.loads(journal.read_text())["domains"].get(broken)
                return value and 0 < value["next"] - time.time() < 40
            eventually(failed_validation, timeout=30)
            assert request(https_port, domain, context=trust)[2] == b"healthy"
            request(dns_management, "localhost", "/clear-a", method="POST", body=json.dumps({"host": broken}).encode())
            eventually(lambda: request(https_port, broken, context=trust)[2] == b"recovered", timeout=50)
            print("PASS: failed HTTP-01 validation backs off and recovers after DNS correction", flush=True)

            # Damage one saved certificate: other sites still start, and the damaged site recovers.
            stop(proxy)
            account_files[0].with_name(f"{broken}.json").write_text("{truncated")
            proxy = start(command, "proxy-corrupt")
            eventually(lambda: request(https_port, domain, context=trust)[2] == b"healthy")
            eventually(lambda: request(https_port, broken, context=trust)[2] == b"recovered", timeout=40)
            print("PASS: corrupt saved certificate is isolated and automatically replaced", flush=True)

            # Force an atomic certificate-save failure after a successful order.
            storage_site = "storage.example.test"
            blocked_file = account_files[0].with_name(f"{storage_site}.json")
            blocked_file.mkdir()
            write_config(f'{domain} {{ respond "healthy" }}\n{storage_site} {{ respond "saved" }}')
            eventually(lambda: "retaining it for another save attempt" in (root / "proxy-corrupt.log").read_text(), timeout=35)
            assert request(https_port, domain, context=trust)[2] == b"healthy"
            def storage_orders():
                return sum("obtaining HTTPS certificate" in line and storage_site in line
                           for line in (root / "proxy-corrupt.log").read_text().splitlines())
            count_before = storage_orders()
            assert count_before == 1
            blocked_file.rmdir()
            eventually(lambda: request(https_port, storage_site, context=trust)[2] == b"saved", timeout=12)
            assert storage_orders() == count_before
            print("PASS: certificate save failure retries retained material while existing HTTPS remains available", flush=True)

            # Suspend the CA through a whole short certificate lifetime; preserve CA account state.
            write_config(f'{domain} {{ respond "healthy" }}')
            eventually(lambda: request(http_port, storage_site)[0] == 404)
            first = fingerprint()
            eventually(lambda: fingerprint() != first, timeout=60)
            ca.send_signal(signal.SIGSTOP)
            try:
                assert request(https_port, domain, context=trust)[2] == b"healthy"
                def expired_tls():
                    try:
                        with connect(https_port, domain, trust):
                            return False
                    except ssl.SSLError:
                        return True
                eventually(expired_tls, timeout=70)
                assert request(http_port, domain)[0] == 308
            finally:
                ca.send_signal(signal.SIGCONT)
            eventually(lambda: request(https_port, domain, context=trust)[2] == b"healthy", timeout=45)
            print("PASS: prolonged CA outage preserves valid HTTPS, rejects expired TLS, and recovers automatically", flush=True)
            renewed = fingerprint()
            stop(proxy)
            stop(ca)
            proxy = start(command, "proxy-restart")
            eventually(lambda: fingerprint() == renewed)
            assert account_files[0].read_bytes() == credentials
            assert request(https_port, domain, context=trust)[0] == 200
            print("PASS: added/removed HTTPS sites, restart with CA offline reuses saved certificate and account", flush=True)
            stop(proxy)
            acme_faults.run(binary, root, processes, cert, key, ca_cert)
        finally:
            for server in backends:
                server.shutdown()
                server.server_close()



if __name__ == "__main__":
    main()
