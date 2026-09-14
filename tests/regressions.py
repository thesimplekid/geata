#!/usr/bin/env python3
"""Focused regression tests using Geata's real listeners and persistent state."""
import argparse
import contextlib
import io
import json
from pathlib import Path
import socketserver
import ssl
import subprocess
import sys
import threading

sys.dont_write_bytecode = True
from support import Processes, eventually, port, request, stop, temporary_directory, tls_fixture
import staging


class ReusedClose(socketserver.BaseRequestHandler):
    def handle(self):
        warmed = False
        stream = self.request.makefile("rb")
        with stream:
            while line := stream.readline():
                method, path, _ = line.decode().split()
                headers = {}
                while (line := stream.readline()) not in (b"\r\n", b"\n", b""):
                    name, value = line.decode().split(":", 1)
                    headers[name.lower()] = value.strip()
                stream.read(int(headers.get("content-length", 0)))
                self.server.requests.append((method, path))
                if path == "/close" and warmed:
                    return  # Close a reused socket before sending any response headers.
                self.request.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                warmed = True


def run(binary, root, group):
    server = socketserver.ThreadingTCPServer(("127.0.0.1", 0), ReusedClose)
    server.daemon_threads = True
    server.requests = []
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        config = root / "Geatafile"
        config.write_text(f'http://localhost {{ respond "old" }}\nhttp://proxy.local {{ reverse_proxy 127.0.0.1:{server.server_address[1]} }}')
        validated = subprocess.run([binary, "validate"], cwd=root, capture_output=True, text=True)
        assert validated.returncode == 0 and "Geatafile is valid" in validated.stdout
        hp, tp = port(), port()
        command = [binary, "run", "--data-dir", str(root / "state"),
                   "--http-listen", f"127.0.0.1:{hp}", "--https-listen", f"127.0.0.1:{tp}"]
        # No --config: this exercises the default filename from the working directory.
        proxy = group.start(command, "regression-proxy", cwd=root)
        eventually(lambda: request(hp, "localhost")[2] == b"old")
        assert request(hp, "proxy.local", "/warm")[0] == 200
        assert request(hp, "proxy.local", "/close")[0] == 200
        assert server.requests.count(("GET", "/close")) == 2, "GET did not retry the closed reused connection"
        assert request(hp, "proxy.local", "/close", method="POST", body=b"one operation")[0] == 502
        assert server.requests.count(("POST", "/close")) == 1, "non-idempotent request was replayed"
        stop(proxy)

        # A relative config path must retain its symlink even after startup.
        first, second = root / "first", root / "second"
        config.rename(first)
        config.symlink_to(first.name)
        second.write_text('http://localhost { respond "new" }')
        proxy = group.start(command, "regression-symlink", cwd=root)
        eventually(lambda: request(hp, "localhost")[2] == b"old")
        replacement = root / "next"
        replacement.symlink_to(second.name)
        replacement.replace(config)
        eventually(lambda: request(hp, "localhost")[2] == b"new", timeout=6)
        # The newly selected target continues to reload normally.
        second.write_text('http://localhost { respond "edited" }')
        eventually(lambda: request(hp, "localhost")[2] == b"edited", timeout=6)
        stop(proxy)

        # Unsupported CA schemes fail before starting listeners or creating state.
        invalid_state = root / "invalid-state"
        invalid = subprocess.run([binary, "run", "--data-dir", str(invalid_state),
                                  "--acme-directory", "http://127.0.0.1:12345/dir"],
                                 cwd=root, capture_output=True, text=True, timeout=5)
        assert invalid.returncode != 0 and "ACME directory must be an HTTPS URL" in invalid.stderr
        assert not invalid_state.exists()
        print("PASS: Geatafile defaults, reused-connection GET retry, no POST replay, symlink reload, and HTTPS-only ACME validation", flush=True)
    finally:
        server.shutdown()
        server.server_close()

    # An offline CA plus a valid saved certificate must not count as new issuance.
    domain = "staging.example.test"
    cert, key, ca = tls_fixture(root, domain)
    trust = ssl.create_default_context(cafile=str(ca))
    data = root / "staging-state"
    data.mkdir(mode=0o700)
    saved = staging.certificate_path(data, domain)
    saved.parent.mkdir(mode=0o700)
    saved.write_text(json.dumps({"chain": cert.read_text(), "private_key": key.read_text()}))
    saved.chmod(0o600)
    previous = staging.saved_fingerprint(saved)
    config = root / "staging.Geatafile"
    config.write_text(f'{domain} {{ respond "{staging.RESPONSE.decode()}" }}')
    hp, tp = port(), port()
    proxy = group.start([binary, "run", "--staging", "--config", str(config), "--data-dir", str(data),
                         "--http-listen", f"127.0.0.1:{hp}", "--https-listen", f"127.0.0.1:{tp}"], "regression-staging")
    output = io.StringIO()
    with contextlib.redirect_stdout(output):
        assert not staging.verify_site(proxy, domain, trust, previous, http_port=hp, https_port=tp, timeout=10)
    assert "SKIP: fresh public HTTP-01 issuance was not tested" in output.getvalue()
    try:
        staging.verify_site(proxy, domain, trust, previous, require_issuance=True, http_port=hp, https_port=tp, timeout=1)
    except AssertionError as error:
        assert "fresh staging issuance was not verified" in str(error)
    else:
        raise AssertionError("strict staging check accepted a reused certificate")
    assert staging.saved_fingerprint(saved) == previous
    stop(proxy)
    print("PASS: cached staging certificates report reuse; strict issuance checks reject reuse without deleting state", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    with temporary_directory("geata-regressions-") as temporary, Processes(Path(temporary)) as group:
        run(str(args.binary.resolve()), Path(temporary), group)


if __name__ == "__main__":
    main()
