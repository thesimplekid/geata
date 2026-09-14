#!/usr/bin/env python3
"""Opt-in public HTTP-01 test against Let's Encrypt staging; retains ACME state."""
import argparse
import hashlib
import json
from pathlib import Path
import ssl
import sys

sys.dont_write_bytecode = True
from support import Processes, eventually, peer_fingerprint, request, temporary_directory


STAGING_DIRECTORY = "https://acme-staging-v02.api.letsencrypt.org/directory"
RESPONSE = b"Geata staging HTTPS works"


def certificate_path(data_dir, domain):
    namespace = hashlib.sha256(STAGING_DIRECTORY.encode()).hexdigest()
    return data_dir / namespace / f"{domain}.json"


def saved_fingerprint(path):
    try:
        chain = json.loads(path.read_text())["chain"]
    except FileNotFoundError:
        return None
    leaf = chain.split("-----END CERTIFICATE-----", 1)[0] + "-----END CERTIFICATE-----\n"
    return hashlib.sha256(ssl.PEM_cert_to_DER_cert(leaf)).digest()


def verify_site(process, domain, trust, previous, *, require_issuance=False,
                http_port=80, https_port=443, timeout=300):
    """Check the served certificate against the snapshot taken before startup."""
    def ready():
        if process.poll() is not None:
            raise RuntimeError(f"Geata exited with status {process.returncode}")
        if request(https_port, domain, context=trust)[2] != RESPONSE:
            return False
        current = peer_fingerprint(https_port, domain, trust)
        return current if not require_issuance or current != previous else False

    try:
        current = eventually(ready, timeout=timeout)
    except AssertionError as error:
        if require_issuance:
            raise AssertionError("fresh staging issuance was not verified; inspect the logs or use a new test hostname while retaining account state") from error
        raise
    status, headers, _ = request(http_port, domain)
    suffix = "" if https_port == 443 else f":{https_port}"
    assert status == 308 and headers["Location"] == f"https://{domain}{suffix}/"
    fresh = current != previous
    if fresh:
        print("PASS: fresh staging certificate, verified chain/hostname, HTTPS response and HTTP redirect", flush=True)
    else:
        print("PASS: reused staging certificate, verified chain/hostname, HTTPS response and HTTP redirect", flush=True)
        print("SKIP: fresh public HTTP-01 issuance was not tested; use --require-issuance with a new test hostname", flush=True)
    return fresh


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--domain", required=True, help="domain pointing to this machine; public TCP 80/443 must reach it")
    parser.add_argument("--data-dir", type=Path, required=True, help="persistent private staging state directory")
    parser.add_argument("--ca-root", type=Path, required=True, help="PEM bundle of official Let's Encrypt staging roots")
    parser.add_argument("--require-issuance", action="store_true", help="fail unless a certificate different from the saved one is served within five minutes")
    args = parser.parse_args()
    # Reject syntax characters before interpolating the host into a Geatafile.
    domain = args.domain.encode("idna").decode("ascii").lower()
    if "." not in domain or any(c not in "abcdefghijklmnopqrstuvwxyz0123456789-." for c in domain):
        parser.error("--domain must be a DNS hostname")
    trust = ssl.create_default_context(cafile=str(args.ca_root))
    previous = saved_fingerprint(certificate_path(args.data_dir, domain))
    with temporary_directory("geata-staging-") as temporary, Processes(Path(temporary)) as group:
        config = Path(temporary) / "Geatafile"
        config.write_text(f'{domain} {{ respond "{RESPONSE.decode()}" }}\n')
        command = [str(args.binary.resolve()), "run", "--staging", "--config", str(config),
                   "--data-dir", str(args.data_dir.resolve())]
        process = group.start(command, "staging")
        verify_site(process, domain, trust, previous, require_issuance=args.require_issuance)


if __name__ == "__main__":
    main()
