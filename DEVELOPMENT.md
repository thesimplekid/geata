# Development

The workspace currently contains one application crate. Configuration, routing,
certificate automation, TLS selection, and file storage are separate modules;
additional crates are unnecessary until there is a reusable API boundary.

Use `nix develop path:.`, then:

```sh
just quick-check
just integration
```

`quick-check` runs formatting, Clippy with warnings denied, and unit tests.
`integration` builds the binary and runs actual HTTP/WebSocket requests plus a
local Pebble ACME server. It validates HTTP-01 challenges, verifies certificate
chains and hostnames, waits for a short-lived certificate to renew, checks config
reloads, and restarts the proxy with the CA offline to verify persistence.
Direct responses are checked over HTTP/1.1 and HTTP/2, including quoting, HEAD,
status-only responses, and reloads between proxy and response handlers.
The suite also injects failed HTTP-01 validation, corrupt certificate files,
certificate-save failures, and a CA outage lasting through certificate expiry.
A separate TLS fault server stalls an order while later domains proceed, then
checks persistent per-domain retries and CA `Retry-After` seconds/HTTP dates
across restarts. Unit tests cover retry limits, cancellation cleanup, and storage
recovery while preserving the previous certificate.
The suite takes roughly four to six minutes and needs only loopback sockets,
not a public domain.
Test CA certificates are trusted only inside the test processes.

For HTTP-only checks, use `just smoke`. For focused tests, use:

```sh
just regressions  # retry safety, config symlinks/defaults, CA URL validation, staging reuse
just faults       # stalled orders and persistent CA rate limits; no Pebble needed
```

The scripts share socket, process, and private TLS fixtures in `tests/support.py`.
The full integration run includes both focused suites.

Public staging issuance is an opt-in test, separate from the reproducible local
gate. It requires a domain whose public DNS points to this machine, reachable
TCP ports 80/443, and permission to bind those ports. Obtain the test roots from
[Let's Encrypt's staging documentation](https://letsencrypt.org/docs/staging-environment/)
and put them in a PEM bundle trusted only by the test process, then run:

```sh
python3 tests/staging.py result/bin/geata \
  --domain YOUR_PUBLIC_DOMAIN \
  --require-issuance \
  --data-dir /data/rust/tmp/geata-staging-state \
  --ca-root /path/to/staging-roots.pem
```

Use a persistent private data directory and reuse it on subsequent runs. The
script runs Geata with `--staging`, waits up to five minutes, verifies the TLS
chain and hostname, checks the direct HTTPS response and HTTP redirect, then
stops its process. It snapshots the saved certificate before startup:
`--require-issuance` fails unless a different certificate is served. Use a new
public test hostname with the existing data directory to test issuance promptly
without deleting account credentials or certificate state. Without that flag,
reuse is reported as a connectivity pass with an explicit issuance skip; it does
not prove public DNS or HTTP-01 reachability. Neither mode deletes saved state.
Public issuance cannot be tested using the reserved example domains in this repository.

The final gate, `just final-check`, runs `nix flake check path:. -L` directly.
Nix checks the package, formatting, Clippy, unit tests, and the complete integration
suite against the release binary. It does not first repeat the long integration
suite against a debug build; `just integration` remains available for that workflow.
Rust's version is pinned in `rust-toolchain.toml`; Nix asserts its Rust version
matches. Update the Nix pin and Rust toolchain together. Cargo dependencies and
Nix inputs are locked in `Cargo.lock` and `flake.lock`.

On this workstation keep builds and temporary data on the dedicated disk:

```sh
export CARGO_TARGET_DIR=/data/rust/targets/geata
export TMPDIR=/data/rust/tmp
nix develop path:.
```

No shell hook changes global state or creates databases. The integration script
creates private temporary directories, stops its own processes, and cleans up.

The explicit `path:.` also works before repository initialization. This workspace
initially contained an empty, read-only `.git` placeholder, not valid Git or JJ
metadata; no repository history has been created or changed.

When changing configuration or CLI options, update their validation, examples,
help, and README together. Changes to certificate persistence must preserve
private permissions and atomic certificate/key replacement. Never log private
keys, ACME account credentials, or challenge responses.
