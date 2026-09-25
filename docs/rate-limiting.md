# Rate limiting

Add a per-client-IP limit to either a proxied site or a direct response:

```caddyfile
example.com {
    rate_limit 10/s burst 20
    reverse_proxy localhost:3000
}
```

## How the allowance works

Each IP gets its own token bucket for each site: it starts with 20 tokens,
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
Each limited site tracks up to 16,384 IPs across 16 bounded shards. Idle buckets
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

Back to [Geata](../README.md).
