# Configuration

Each site has exactly one `reverse_proxy` or `respond` directive, with optional
`rate_limit`, `pay`, `lightning_pay`, `lightning_protocols`, `lightning_headers`,
`lightning_origin`, `max_inflight`, `max_body_bytes`, `request_timeout`,
`ipv6_prefix`, and `payment_verify_limit` settings. Braces can
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

## Payment and free allowances

Use `pay` for Cashu or `lightning_pay` for Lightning. Without `rate_limit`,
every request to the site's handler requires payment:

```caddyfile
example.com {
    pay 2 sat https://mint.example.com
    reverse_proxy localhost:3000
}
```

Add `rate_limit 10/s burst 20` to grant each IP a free allowance before requiring
payment. Without either payment directive, an exhausted rate limit returns 429.
Without payment directives or a rate limit, requests are unrestricted by either.

## Direct responses

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

## Proxy behavior and supported syntax

Backend addresses default to HTTP. HTTPS backends verify the certificate and
hostname, and receive their own hostname in `Host`; the original host is sent in
`X-Forwarded-Host`. Paths, query strings, streaming bodies, and WebSocket upgrades
are forwarded. The client-facing HTTPS listener supports HTTP/1.1 and HTTP/2;
backend connections currently use HTTP/1.1. Proxying gRPC is not supported; the Lightning receiver uses a separate gRPC client.

The parser deliberately supports this small syntax, not the full Caddyfile
language. Unsupported directives, duplicate sites, wildcard hosts, local HTTPS,
backend URL credentials/paths, and site-specific listener ports are rejected.

## Files and reloads

The default filename is `Geatafile`. Existing configurations with another name
can still be loaded with `--config`, for example `geata run --config Proxyfile`.

Edit the `Geatafile` to add or change sites. The proxy checks it every two seconds,
applies valid changes to new requests, and lets in-flight requests finish. Invalid
edits produce an error in the logs and leave the previous configuration running.
Atomic symlink swaps are supported; reloads follow the configured path.
Write edits atomically (save to a temporary file, then rename) to avoid loading an
intermediate but syntactically valid version during a multi-step edit.

An optional top-level `cashu_payout { ... }` block configures scheduled or
balance-triggered [operator payouts](payouts.md), one block per mint.

A top-level `lightning <name> { ... }` block defines a shared LDK Server or Cashu mint receiver.
Sites reference its name with `lightning_pay` and select their own bound
headers with `lightning_headers`. Mint receivers require `lightning_protocols l402`.
See [Lightning payments](lightning.md) for the
complete syntax and credential paths.

For request controls, see [rate limiting](rate-limiting.md) and
[Cashu payments](cashu.md), and [Lightning payments](lightning.md).

Back to [Geata](../README.md).
