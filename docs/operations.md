# HTTPS and operation

## Public domains

For a public domain, point its DNS records at your server and allow inbound TCP
ports 80 and 443. The process needs permission to bind those ports. The proxy
obtains a Let's Encrypt certificate, redirects HTTP to HTTPS, saves the
certificate, and renews it automatically. No Certbot or renewal cron job is needed.
Running with public domains registers a certificate account and accepts the
configured certificate authority's terms of service. An account email is optional:
`geata run --email admin@example.com`.

Public certificates use ACME HTTP-01 validation on external port **80**. This
version does not yet implement TLS-ALPN-01 fallback, DNS challenges, wildcard
certificates, issuer fallback, or Caddy's local certificate authority.

## Issuance and renewal

First issuance happens in the background. HTTPS is unavailable for a new domain
until its certificate is ready; logs report success or the reason for failure.
Existing sites continue to serve while another domain is being provisioned.

Certificates renew after two-thirds of their lifetime. A replacement is saved
atomically before it is installed in memory. Failed requests use exponential
backoff with jitter, normally starting around 30 seconds and capped at one day.
Renewal retries shorten as expiry approaches, with a five-second minimum.
Retry deadlines survive restarts; interrupted orders have a persisted wait.
CA `Retry-After` deadlines take precedence over urgent renewals and pause all
requests to that CA, including after restart. A still-valid certificate remains
available during failures; expired certificates are not served.

Up to four issuance workers run concurrently. Each free slot can start another
domain immediately, and removed sites have their pending work cancelled.

## Storage and recovery

State defaults to `$XDG_DATA_HOME/geata`, or
`$HOME/.local/share/geata`. Override with `--data-dir` or `PROXY_DATA_DIR`.
The directory must be private (0700), writable, and persistent. Certificate and
account files are created with mode 0600. Back up this directory securely.

A data directory belongs to one running process. A lock prevents simultaneous
writers. Staging and production use separate storage namespaces.

Upgrading from the original `pingora-proxy` name automatically reuses its
existing data directory if the new `geata` directory does not exist. No keys or
certificates are moved. `--data-dir` and `PROXY_DATA_DIR` still take precedence.

A corrupt saved certificate is reported and replaced without preventing other
sites from starting. Failed certificate/account saves retain the new material
in memory for another save attempt. New CA work pauses if retry state cannot
be persisted. Keep storage writable; unsaved material cannot survive a crash.
Corrupt account or retry files require operator repair from a trusted backup;
Geata does not silently discard account identities or recorded CA cooldowns.
Monitor logs for issuance, renewal, and storage failures.

## Staging and private certificate authorities

Test public issuance with `--staging` first. Staging certificates are not
browser-trusted. Private ACME servers can use an HTTPS `--acme-directory` and
`--acme-root` to supply a dedicated trusted CA certificate. HTTP directory URLs
are rejected at startup.

Listener overrides are useful for testing or port forwarding. HTTP-01 still
requires external port 80. Redirects use the configured HTTPS listener port.

## Forwarding and shutdown

Forwarding headers are replaced using the connected client's address. There
is no trusted-CDN/proxy configuration yet; deploy directly at the internet edge.

Backend DNS is resolved per request, with connection pooling and retries across
resolved addresses on connection failure. Connection and I/O timeouts are fixed
for this first version (10 seconds to connect; 5 minutes read/write inactivity).

Unknown hosts return 404; unknown or removed TLS names fail the TLS handshake.
SIGTERM shuts down gracefully with a bounded drain period.

`RUST_LOG` controls log verbosity. Trace-level events from Pingora's proxy layer
are always suppressed because they contain complete request headers, including
credentials and bearer tokens; debug-level dependency diagnostics remain
available.

## Current scope

This is a working foundation, not yet a feature-complete or hardened Caddy
replacement. Load balancing, configurable policies, HTTP/3, broader deployment
testing, and operational metrics are future work.

See [DEVELOPMENT.md](../DEVELOPMENT.md) for checks and local ACME integration tests.

Back to [Geata](../README.md).
