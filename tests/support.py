"""Shared socket, process, and TLS fixtures for Geata's executable tests."""
import hashlib
import http.client
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def eventually(check, timeout=20):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (OSError, AssertionError, http.client.HTTPException) as error:
            last = error
        time.sleep(0.2)
    raise AssertionError(f"condition timed out: {last}")


def connect(port_number, domain, context=None):
    sock = socket.create_connection(("127.0.0.1", port_number), timeout=8)
    if context:
        sock = context.wrap_socket(sock, server_hostname=domain)
    return sock


def request(port_number, domain, path="/", context=None, method="GET", body=b"", headers=None):
    with connect(port_number, domain, context) as sock:
        extra = "".join(f"{key}: {value}\r\n" for key, value in (headers or {}).items())
        sock.sendall((f"{method} {path} HTTP/1.1\r\nHost: {domain}\r\n"
                      f"Content-Length: {len(body)}\r\nConnection: close\r\n{extra}\r\n").encode() + body)
        response = http.client.HTTPResponse(sock, method=method)
        response.begin()
        return response.status, dict(response.getheaders()), response.read()


def temporary_directory(prefix):
    scratch = os.environ.get("TMPDIR", "/data/rust/tmp" if Path("/data/rust/tmp").is_dir() else None)
    return tempfile.TemporaryDirectory(prefix=prefix, dir=scratch)


def stop(process):
    if process.poll() is None:
        process.send_signal(signal.SIGTERM)
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


class Processes:
    def __init__(self, root):
        self.root = root
        self.processes = []
        self.logs = []

    def __enter__(self):
        return self

    def start(self, command, name, env=None, cwd=None):
        log = open(self.root / f"{name}.log", "ab")
        self.logs.append(log)
        process = subprocess.Popen(command, stdout=log, stderr=log, env=env, cwd=cwd)
        self.processes.append(process)
        return process

    def __exit__(self, exc_type, *_):
        for process in reversed(self.processes):
            stop(process)
        for log in self.logs:
            log.close()
        if exc_type is not None:
            for path in self.root.glob("*.log"):
                print(f"\n--- {path.name} ---\n{path.read_text(errors='replace')[-18000:]}")


def peer_fingerprint(port_number, domain, context):
    with connect(port_number, domain, context) as sock:
        return hashlib.sha256(sock.getpeercert(binary_form=True)).digest()


def tls_fixture(root, domain="localhost"):
    # A private bootstrap CA for the local ACME server, never installed in system trust.
    cert, key = root / "acme.pem", root / "acme.key"
    ca_cert, ca_key = root / "bootstrap-root.pem", root / "bootstrap-root.key"
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                    "-subj", "/CN=Test ACME Root", "-addext", "basicConstraints=critical,CA:TRUE",
                    "-addext", "keyUsage=critical,keyCertSign,cRLSign",
                    "-keyout", str(ca_key), "-out", str(ca_cert)],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    csr, extensions = root / "acme.csr", root / "extensions.cnf"
    extensions.write_text(f"subjectAltName=DNS:{domain},IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
    subprocess.run(["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=localhost",
                    "-keyout", str(key), "-out", str(csr)], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(["openssl", "x509", "-req", "-in", str(csr), "-CA", str(ca_cert), "-CAkey", str(ca_key),
                    "-CAcreateserial", "-days", "1", "-extfile", str(extensions), "-out", str(cert)],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return cert, key, ca_cert
