# Geata

Geata (Irish for “gate”) is a small Rust reverse proxy built on Pingora, with a
Caddy-style configuration and automatic HTTPS.
This is an initial implementation for a single Linux server.

Write a `Geatafile`:

```caddyfile
example.com {
    reverse_proxy localhost:3000
}
```

Run:

```sh
geata validate
geata run
```

For a public domain, point its DNS records at your server and allow inbound TCP
ports 80 and 443. The process needs permission to bind those ports. The proxy
obtains a Let's Encrypt certificate, redirects HTTP to HTTPS, saves the
certificate, and renews it automatically. No Certbot or renewal cron job is needed.
Running with public domains registers a certificate account and accepts the
configured certificate authority's terms of service. An account email is optional:
`geata run --email admin@example.com`.

The default filename is `Geatafile`. Existing configurations with another name
can still be loaded with `--config`, for example `geata run --config Proxyfile`.

Edit the `Geatafile` to add or change sites. The proxy checks it every two seconds,
applies valid changes to new requests, and lets in-flight requests finish. Invalid
edits produce an error in the logs and leave the previous configuration running.
Atomic symlink swaps are supported; reloads follow the configured path.
Write edits atomically (save to a temporary file, then rename) to avoid loading an
intermediate but syntactically valid version during a multi-step edit.

## Build and try locally

With Nix:

```sh
nix develop path:.
cargo build --locked
```

Or run `nix build path:.` for `result/bin/geata`.
Without Nix, install the Rust version in `rust-toolchain.toml`, a C/C++ compiler,
CMake, pkg-config, and OpenSSL development libraries, then run
`OPENSSL_NO_VENDOR=1 cargo build --locked` to use your system OpenSSL.

Run the included Hello world example on unprivileged ports; no backend is needed:

```sh
cargo run -- run --config examples/Geatafile.local \
  --http-listen 127.0.0.1:8080 --https-listen 127.0.0.1:8443
```

In another terminal:

```sh
curl http://localhost:8080
```

The explicit `http://` site address disables certificate requests for that site.
Both listeners are opened so a reload can introduce HTTPS sites later.

## Configuration

Each site has exactly one `reverse_proxy` or `respond` directive. Braces can
appear on one line or multiple lines. Blank lines and `#` comments outside quoted
strings are supported.

```caddyfile
example.com {
    reverse_proxy localhost:3000
}

api.example.com {
    reverse_proxy https://backend.example.net:8443
}

http://localhost {
    respond "Hello world!"
}
```

To serve a response directly over automatic HTTPS:

```caddyfile
hello.example.com {
    respond "Hello world!"
}
```

Replace the domain with your own and run the proxy as usual. `respond` returns
the configured text directly, with status 200 and content type
`text/plain; charset=utf-8`. It uses the same certificate automation and HTTP
redirects as proxied sites. No extra server is needed.

An optional status code follows the body: `respond "Unavailable" 503`.
Use `respond 204` for an empty response, or `respond ""` for an empty 200 response.
Quote messages containing spaces; quoted strings support JSON escapes such as
`\n`, `\"`, and `\\`. Braces and `#` inside quotes are literal text. A quoted
number such as `respond "404"` is a body, whereas `respond 404` is a status code.
Only final status codes 200–599 are accepted. Statuses 204, 205, and 304 cannot
have a body. HEAD requests return the headers without the body.

This small `respond` implementation always sends plain text; matchers, custom
headers, automatic JSON content types, heredocs, and nested response blocks are
not supported yet.

Backend addresses default to HTTP. HTTPS backends verify the certificate and
hostname, and receive their own hostname in `Host`; the original host is sent in
`X-Forwarded-Host`. Paths, query strings, streaming bodies, and WebSocket upgrades
are forwarded. The client-facing HTTPS listener supports HTTP/1.1 and HTTP/2;
backend connections currently use HTTP/1.1. There is no gRPC support in this version.

The parser deliberately supports this small syntax, not the full Caddyfile
language. Unsupported directives, duplicate sites, wildcard hosts, local HTTPS,
backend URL credentials/paths, and site-specific listener ports are rejected.

## Certificates and operation

- Public certificates use ACME HTTP-01 validation on external port **80**. This
  version does not yet implement TLS-ALPN-01 fallback, DNS challenges, wildcard
  certificates, issuer fallback, or Caddy's local certificate authority.
- First issuance happens in the background. HTTPS is unavailable for a new domain
  until its certificate is ready; logs report success or the reason for failure.
  Existing sites continue to serve while another domain is being provisioned.
- Certificates renew after two-thirds of their lifetime. A replacement is saved
  atomically before it is installed in memory. Failed requests use exponential
  backoff with jitter, normally starting around 30 seconds and capped at one day.
  Renewal retries shorten as expiry approaches, with a five-second minimum.
  Retry deadlines survive restarts; interrupted orders have a persisted wait.
  CA `Retry-After` deadlines take precedence over urgent renewals and pause all
  requests to that CA, including after restart. A still-valid certificate remains
  available during failures; expired certificates are not served.
- Up to four issuance workers run concurrently. Each free slot can start another
  domain immediately, and removed sites have their pending work cancelled.
- A corrupt saved certificate is reported and replaced without preventing other
  sites from starting. Failed certificate/account saves retain the new material
  in memory for another save attempt. New CA work pauses if retry state cannot
  be persisted. Keep storage writable; unsaved material cannot survive a crash.
  Corrupt account or retry files require operator repair from a trusted backup;
  Geata does not silently discard account identities or recorded CA cooldowns.
  Monitor logs for issuance, renewal, and storage failures.
- State defaults to `$XDG_DATA_HOME/geata`, or
  `$HOME/.local/share/geata`. Override with `--data-dir` or `PROXY_DATA_DIR`.
  The directory must be private (0700), writable, and persistent. Certificate and
  account files are created with mode 0600. Back up this directory securely.
- Upgrading from the original `pingora-proxy` name automatically reuses its
  existing data directory if the new `geata` directory does not exist. No keys or
  certificates are moved. `--data-dir` and `PROXY_DATA_DIR` still take precedence.
- A data directory belongs to one running process. A lock prevents simultaneous
  writers. Staging and production use separate storage namespaces.
- Test public issuance with `--staging` first. Staging certificates are not
  browser-trusted. Private ACME servers can use an HTTPS `--acme-directory` and
  `--acme-root` to supply a dedicated trusted CA certificate. HTTP directory URLs
  are rejected at startup.
- Listener overrides are useful for testing or port forwarding. HTTP-01 still
  requires external port 80. Redirects use the configured HTTPS listener port.
- Forwarding headers are replaced using the connected client's address. There
  is no trusted-CDN/proxy configuration yet; deploy directly at the internet edge.
- Backend DNS is resolved per request, with connection pooling and retries across
  resolved addresses on connection failure. Connection and I/O timeouts are fixed
  for this first version (10 seconds to connect; 5 minutes read/write inactivity).
- Unknown hosts return 404; unknown or removed TLS names fail the TLS handshake.
  SIGTERM shuts down gracefully with a bounded drain period.

This is a working foundation, not yet a feature-complete or hardened Caddy
replacement. Load balancing, configurable policies, HTTP/3, broader deployment
testing, and operational metrics are future work.

See [DEVELOPMENT.md](DEVELOPMENT.md) for checks and local ACME integration tests.
