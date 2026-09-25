# Rate limiting

Add a per-client-IP limit to either a proxied site or a direct response:

```caddyfile
example.com {
    rate_limit 10/s burst 20
    reverse_proxy localhost:3000
}
```

## How the allowance works

Each client identity gets its own token bucket for each site: it starts with 20 tokens,
refills at 10 tokens per second, and holds at most 20. Every request to the
handler consumes one token, regardless of path or method. Exhausted buckets
receive **429 Too Many Requests** with `Retry-After` in whole seconds and
`Cache-Control: no-store`. Rejected requests do not reach the backend.
Rates and burst sizes must be positive integers up to 4294967295; this first
version accepts `/s` only and requires `burst`. Omit `rate_limit` to disable it.

## Client identity

Limits use the connected client's IP, shared across worker threads. Forwarded
headers cannot change the allowance. People sharing a public IP share its
allowance; a CDN or another proxy in front of Geata also shares its connection
IP's allowance, since trusted-proxy configuration is not yet supported.
HTTP-to-HTTPS redirects and HTTP-01 challenge handling are exempt. WebSocket
handshakes count as requests; individual messages do not.

## Reloads and memory limits

Unchanged limits keep their buckets across configuration reloads, including
handler changes. Changing a limit, removing/re-adding it or its site, or restarting
Geata starts fresh buckets. Each Geata process enforces its own independent limits.
Each limited site tracks up to 16,384 client identities across 16 bounded shards. Idle buckets
are reclaimed after at least a minute, once fully refilled. If a shard is full,
new IPs assigned to it receive 429 with `Retry-After: 60` until space is available;
existing allowances are preserved.

## Try it locally

Try [examples/Geatafile.rate-limit](../examples/Geatafile.rate-limit) locally:

```sh
geata run --config examples/Geatafile.rate-limit \
  --http-listen 127.0.0.1:8080 --https-listen 127.0.0.1:8443
```

From another terminal, repeated requests show the burst and subsequent 429s:

```sh
for i in $(seq 1 10); do curl -i http://localhost:8080/; done
```

To offer paid access when the free allowance is exhausted, see
[Cashu payments](cashu.md).
With `pay` or `lightning_pay`, omitting `rate_limit` requires payment for every
request. Zero rates and burst sizes remain invalid.


## Resource and payment-verification controls

Every site defaults to `max_inflight 128`, including sites without payments or
rate limits. At most half the capacity can belong to one client identity (at
least one slot). Capacity remains held through response streaming and upgrades;
exhaustion returns 503. This is also a site-wide concurrency admission bound.

```caddyfile
example.com {
    reverse_proxy localhost:3000
    max_inflight 128
    max_body_bytes 10485760
    request_timeout 60
    ipv6_prefix 64
    payment_verify_limit 2/s burst 8
}
```

These values are the defaults. `max_body_bytes` counts HTTP request body bytes,
including chunked uploads. Known oversized Content-Length values receive 413
before payment processing; streamed bodies are stopped when they exceed the
limit. A backend may already have received the allowed prefix of a streamed body.
Lightning binding also retains its stricter 64 KiB limit.

`request_timeout` is a total duration in seconds after routing, covering payment
work, upstream connection attempts, uploads, responses, and WebSocket lifetimes.
Expiration closes the HTTP/1 connection or cancels the HTTP/2 stream and releases
capacity; it does not promise an HTTP error response. Headers must arrive within
60 seconds. A timeout after payment processing starts does not imply that a
payment or backend operation was rolled back.

Use positive integers to raise limits. `max_body_bytes off`, `request_timeout off`,
and `max_inflight off` disable the respective bound explicitly. Streaming and
WebSocket services should use separate sites with appropriate settings; route
matchers are not supported. Body limits do not count upgraded WebSocket frames.

IPv4-mapped addresses share their IPv4 identity. IPv6 clients share allowances,
payment-verification buckets, and capacity by prefix, default `/64`. Set
`ipv6_prefix` to a length from 0 to 128; 128 restores individual-address buckets.
Rotating addresses within one prefix consumes only one table entry. Distinct
prefixes can still exhaust the bounded table, so deployments exposed to a large
distributed source should also enforce admission limits at their network edge.

`payment_verify_limit` uses the same rate/burst syntax as `rate_limit`, but has an
independent bucket. Supplied payment credentials, including malformed ones, are
charged before body buffering, cryptography, storage or mint calls. Exhaustion
returns 429 with Retry-After. Valid payments remain exempt from the free allowance,
but are subject to verification and resource limits. Unchanged verification
limits survive reloads; changing the IPv6 prefix resets client buckets.

## Production logging

Routine rejections and failed requests are counted by HTTP status in a fixed-size,
process-wide table. A summary is emitted at most once every ten seconds when
rejected traffic arrives, without client-supplied hosts, paths, or credentials.
Status 0 includes failures without a response and upstream retry warnings; 408
includes total-duration expirations. The summary resets the interval counters.
Successful request logs remain enabled.

Geata writes to standard output; run it with a bounded log collector. For systemd
services, use the journal rather than appending output to an unrotated file.
For example, install `/etc/systemd/journald.conf.d/storage-limits.conf`:

```ini
[Journal]
SystemMaxUse=512M
RuntimeMaxUse=128M
MaxRetentionSec=7day
RateLimitIntervalSec=30s
RateLimitBurst=1000
```

Apply the journal configuration through your deployment tooling. These settings
bound the host journal, not only Geata. Container deployments need equivalent
size and file-count limits in their logging driver. Application sampling does
not replace storage rotation, including for dependency logs or successful traffic.

Back to [Geata](../README.md).
