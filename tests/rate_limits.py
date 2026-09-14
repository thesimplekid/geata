#!/usr/bin/env python3
"""Exercise per-IP rate limits through the real HTTP and HTTPS listeners."""
import argparse
import hashlib
import http.client
import http.server
import json
from pathlib import Path
import ssl
import sys
import threading
import time

sys.dont_write_bytecode = True
from support import Processes, eventually, port, request, stop, temporary_directory, tls_fixture


class Backend(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.server.requests += 1
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *_):
        pass


def from_ip(port_number, domain, ip):
    connection = http.client.HTTPConnection("127.0.0.1", port_number, timeout=5, source_address=(ip, 0))
    try:
        connection.request("GET", "/", headers={"Host": domain})
        response = connection.getresponse()
        response.read()
        return response.status
    finally:
        connection.close()


def exhaust(fetch):
    for _ in range(100):
        status, headers, body = fetch()
        if status == 429:
            assert int(headers["Retry-After"]) >= 1
            assert headers["Cache-Control"] == "no-store"
            return headers
        assert status == 200, (status, body)
    raise AssertionError("rate limit did not reject a request")


def run(binary, root, group):
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Backend)
    server.requests = 0
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        domain = "limited.example.test"
        cert, key, ca = tls_fixture(root, domain)
        trust = ssl.create_default_context(cafile=str(ca))
        directory = f"https://localhost:{port()}/dir"
        data = root / "state"
        data.mkdir(mode=0o700)
        saved = data / hashlib.sha256(directory.encode()).hexdigest()
        saved.mkdir(mode=0o700)
        pem = saved / f"{domain}.json"
        pem.write_text(json.dumps({"chain": cert.read_text(), "private_key": key.read_text()}))
        pem.chmod(0o600)
        config = root / "Geatafile"

        def write_config(generation, directive="rate_limit 1/s burst 20"):
            text = f'''http://ready.local {{ respond "{generation}" }}
http://direct.local {{
    {directive}
    respond "hello"
}}
http://proxy.local {{
    rate_limit 1/s burst 3
    reverse_proxy 127.0.0.1:{server.server_port}
}}
{domain} {{
    rate_limit 1/s burst 3
    respond "secure"
}}
'''
            replacement = config.with_suffix(".new")
            replacement.write_text(text)
            replacement.replace(config)

        write_config("initial")
        hp, tp = port(), port()
        proxy = group.start([binary, "run", "--config", str(config), "--data-dir", str(data),
                             "--http-listen", f"127.0.0.1:{hp}", "--https-listen", f"127.0.0.1:{tp}",
                             "--acme-directory", directory], "rate-limit-proxy")
        eventually(lambda: request(hp, "ready.local")[2] == b"initial")
        direct = lambda: request(hp, "direct.local")
        exhaust(direct)
        status, headers, body = request(hp, "direct.local", "/another-path", method="HEAD")
        assert status == 429 and body == b"" and int(headers["Content-Length"]) > 0
        assert request(hp, "direct.local", headers={"X-Forwarded-For": "198.51.100.1",
                                                   "X-Real-IP": "198.51.100.2",
                                                   "Forwarded": "for=198.51.100.3"})[0] == 429
        assert from_ip(hp, "direct.local", "127.0.0.2") == 200, "another IP shared the depleted allowance"
        assert request(hp, "proxy.local")[0] == 200, "another site shared the depleted allowance"
        assert request(hp, "ready.local")[0] == 200
        assert request(hp, "unknown.local")[0] == 404
        headers = exhaust(direct)
        time.sleep(int(headers["Retry-After"]))
        assert direct()[0] == 200, "allowance did not refill after Retry-After"

        exhaust(lambda: request(hp, "proxy.local"))
        before = server.requests
        assert request(hp, "proxy.local", method="POST", body=b"do not forward")[0] == 429
        assert server.requests == before, "rejected request reached the backend"

        # Redirects must not consume the HTTPS allowance, and challenges bypass the limiter.
        for _ in range(5):
            assert request(hp, domain)[0] == 308
        assert request(tp, domain, context=trust)[0] == 200
        exhaust(lambda: request(tp, domain, context=trust))
        assert request(hp, domain)[0] == 308
        assert request(hp, domain, "/.well-known/acme-challenge/not-active")[0] == 404

        exhaust(direct)
        write_config("unchanged")
        eventually(lambda: request(hp, "ready.local")[2] == b"unchanged", timeout=6)
        # At most six seconds of refill; a reset would instead allow all twenty.
        statuses = [direct()[0] for _ in range(20)]
        assert 429 in statuses, "unrelated config reload reset the allowance"
        write_config("changed", "rate_limit 1/s burst 2")
        eventually(lambda: request(hp, "ready.local")[2] == b"changed", timeout=6)
        assert [direct()[0] for _ in range(3)] == [200, 200, 429]
        write_config("disabled", "")
        eventually(lambda: request(hp, "ready.local")[2] == b"disabled", timeout=6)
        assert all(direct()[0] == 200 for _ in range(25))
        stop(proxy)
        print("PASS: per-IP/site limits, refill, 429/Retry-After/HEAD, spoofed headers, backend protection, TLS, redirects, and reloads", flush=True)
    finally:
        server.shutdown()
        server.server_close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    with temporary_directory("geata-rate-limits-") as temporary, Processes(Path(temporary)) as group:
        run(str(args.binary.resolve()), Path(temporary), group)


if __name__ == "__main__":
    main()
