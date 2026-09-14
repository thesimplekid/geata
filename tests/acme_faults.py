"""Deterministic ACME transport faults over verified TLS, with the real binary."""
import argparse
import base64
from pathlib import Path
import sys
from email.utils import formatdate
import http.server
import json
import ssl
import threading
import time


sys.dont_write_bytecode = True
from support import Processes, eventually, port, stop, temporary_directory, tls_fixture


class FaultCA(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_HEAD(self):
        self.do_GET()

    def reply(self, status, body, **headers):
        content = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(content)))
        self.send_header("Replay-Nonce", "bm9uY2U")
        for name, value in headers.items():
            self.send_header(name.replace("_", "-"), value)
        self.end_headers()
        if self.command != "HEAD":
            try:
                self.wfile.write(content)
            except (OSError, ssl.SSLError):
                pass  # The scheduler may cancel the stalled request.

    def do_GET(self):
        self.server.requests.append(time.time())
        base = f"https://localhost:{self.server.server_port}"
        if self.path == "/dir":
            return self.reply(200, {"newNonce": base + "/nonce", "newAccount": base + "/account", "newOrder": base + "/order"})
        self.reply(200, {})

    def do_POST(self):
        self.server.requests.append(time.time())
        encoded = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        payload = json.loads(base64.urlsafe_b64decode(encoded["payload"] + "==="))
        if self.path == "/account":
            return self.reply(201, {"status": "valid"}, Location=f"https://localhost:{self.server.server_port}/account/1")
        domain = payload["identifiers"][0]["value"]
        self.server.orders.append((domain, time.time()))
        if domain.startswith("a-stalled"):
            self.server.release.wait(timeout=60)
        if self.server.rate_limit:
            if self.server.deadline is None:
                self.server.deadline = int(time.time()) + 16
            retry = str(max(1, self.server.deadline - int(time.time())))
            if self.server.rate_limit == "date":
                retry = formatdate(self.server.deadline, usegmt=True)
            return self.reply(429, {"type": "urn:ietf:params:acme:error:rateLimited", "detail": "injected rate limit", "status": 429}, Retry_After=retry)
        self.reply(403, {"type": "urn:ietf:params:acme:error:unauthorized", "detail": "injected order failure", "status": 403})


def run(binary, root, group, cert, key, ca_cert):
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FaultCA)
    server.requests, server.orders = [], []
    server.release = threading.Event()
    server.rate_limit, server.deadline = None, None
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(cert, key)
    server.socket = tls.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    processes = []
    try:
        def launch(name, domains):
            directory = root / name
            directory.mkdir(exist_ok=True)
            config = directory / "Geatafile"
            config.write_text("\n".join(f'{domain} {{ respond "ready" }}' for domain in domains))
            command = [binary, "run", "--config", str(config), "--data-dir", str(directory / "state"),
                       "--http-listen", f"127.0.0.1:{port()}", "--https-listen", f"127.0.0.1:{port()}",
                       "--acme-directory", f"https://localhost:{server.server_port}/dir", "--acme-root", str(ca_cert)]
            process = group.start(command, name)
            processes.append(process)
            return process, directory

        domains = [f"{prefix}.example.test" for prefix in ("a-stalled", "b", "c", "d", "e", "f")]
        process, directory = launch("scheduler-faults", domains)
        eventually(lambda: len(server.orders) >= 6, timeout=15)
        assert not server.release.is_set(), "stalled worker was released too early"
        assert {name for name, _ in server.orders} == set(domains)
        # All quick failures acquire a durable retry; a restart does not issue again.
        retry_path = eventually(lambda: next((directory / "state").glob("*/retries.json"), None))
        eventually(lambda: len(json.loads(retry_path.read_text())["domains"]) == 6)
        stop(process)
        count = len(server.orders)
        process, _ = launch("scheduler-faults", domains)
        time.sleep(5)
        assert len(server.orders) == count, "restart reset per-domain retry deadlines"
        stop(process)
        server.release.set()
        print("PASS: a stalled order does not block later domains; failed/crashed orders keep retry deadlines across restart", flush=True)

        for mode in ("seconds", "date"):
            server.requests.clear()
            server.orders.clear()
            server.rate_limit, server.deadline = mode, None
            process, directory = launch(f"rate-{mode}", ["limited.example.test"])
            eventually(lambda: len(server.orders) == 1)
            cooldown_file = eventually(lambda: next((directory / "state").glob("*/ca-retry.json"), None))
            deadline = eventually(lambda: (value if (value := json.loads(cooldown_file.read_text())) > time.time() else None))
            stop(process)
            count = len(server.requests)
            # Fresh domains must also respect the CA-wide cooldown after restart.
            process, _ = launch(f"rate-{mode}", ["limited.example.test", "fresh.example.test"])
            time.sleep(5)
            assert len(server.requests) == count, "request escaped persistent CA cooldown"
            server.rate_limit = None
            eventually(lambda: len(server.orders) >= 2, timeout=25)
            assert server.orders[1][1] >= deadline, "retried before Retry-After deadline"
            stop(process)
        print("PASS: Retry-After seconds and HTTP dates pause all CA requests across restart, then resume", flush=True)
    finally:
        server.release.set()
        for process in processes:
            stop(process)
        server.shutdown()
        server.server_close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    with temporary_directory("geata-faults-") as temporary, Processes(Path(temporary)) as group:
        root = Path(temporary)
        run(str(args.binary.resolve()), root, group, *tls_fixture(root))


if __name__ == "__main__":
    main()
