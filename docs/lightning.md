# Lightning payments with LDK Server

Geata can offer Lightning x402 payments alongside Cashu, or on their own.
Run a separate [LDK Server](https://github.com/lightningdevkit/ldk-server) instance
with incoming Lightning liquidity. Geata creates invoices through its authenticated
TLS gRPC API. Funds stay on that node; Cashu funds continue to use Geata's mint wallet.

The adapter targets `api.LightningNode/Bolt11Receive` with the description-hash
variant of `Bolt11InvoiceDescription`, as defined at LDK Server commit
[`db345786`](https://github.com/lightningdevkit/ldk-server/tree/db3457867ef20fdc0523d882c1585dbdb3e41760).
Older versions without this API are unsupported. Upstream currently describes
LDK Server as experimental and not ready for production use.

## Configuration

Define a named receiver and reference it from each paid site in the Geatafile:

```caddyfile
lightning my_node {
    endpoint https://localhost:3536
    api_key_file /var/lib/ldk-server/bitcoin/api_key
    tls_cert_file /var/lib/ldk-server/tls.crt
    network mainnet
    pay_to YOUR_66_CHARACTER_COMPRESSED_NODE_PUBLIC_KEY
    expiry_seconds 300
}

example.com {
    rate_limit 10/s burst 20
    pay_over_limit 2 sat https://mint.example.com
    lightning_over_limit 2 sat my_node
    lightning_headers accept authorization content-encoding content-type cookie range
    reverse_proxy localhost:3000
}
```

Use the actual node ID for `pay_to` and paths from your LDK Server deployment.
The API key file contains LDK Server's **32 raw bytes**, not a hex string. Give
Geata read access to that key and certificate, keeping the key private. The TLS
certificate must match the endpoint hostname; certificate verification is mandatory.
Credential paths must be absolute. The endpoint must be an HTTPS origin.

Receiver names are case-sensitive and contain letters, digits, underscores, or
hyphens. Definitions may appear before or after their sites. Multiple sites can
share one named receiver while keeping separate prices, origins, and bound headers.
Duplicate names, duplicate or unknown receiver settings, missing required settings,
and references to undefined receivers are rejected by `geata validate`.

`lightning_over_limit <price> sat <receiver name>` requires `rate_limit` and
`lightning_headers`. Prices are positive whole sats, converted exactly to
millisatoshis on the wire. Cashu and Lightning prices may differ. Omit
`pay_over_limit` for Lightning only. The default `max_inflight` is 128 when either
payment method is enabled.

`lightning_headers <names...>` explicitly selects the site's bound headers in
lowercase sorted order. Use `lightning_headers none` only when no header affects
the purchased operation, content interpretation, or account selection. These
settings belong to the site, not the receiver. See request binding below.

The public origin defaults to the site's scheme and domain, such as
`https://example.com`. For a nonstandard listener port, set it on the site:

```caddyfile
http://localhost {
    rate_limit 1/s burst 1
    lightning_over_limit 2 sat my_node
    lightning_headers none
    lightning_origin http://localhost:8080
    respond "Hello world!"
}
```

`lightning_origin` must exactly match the public scheme and authority, including
any nondefault port, without a trailing slash. Its hostname and HTTPS setting
must match the site. Public paid requests require HTTPS. Forwarded headers are
not trusted for origin reconstruction; deploy Geata at the public TLS endpoint.

The receiver's `network` is required and accepts `mainnet` or `testnet`, mapped
on the x402 wire to `lnbtc:000000000019d6689c085ae165831e93` or
`lnbtc:000000000933ea01ad0ee984209779ba`, respectively. Signet, regtest, and
testnet4 are unsupported. `expiry_seconds` is optional, defaults to 300, and
accepts values from 1 to 86400.

All receiver and site settings reload together when the Geatafile changes.
Invalid edits leave the previous configuration running. Credential files are read
when issuing invoices, so replacing them does not require a reload. No separate
JSON configuration file is used.

## Payment flow

After the free allowance runs out, a 402 response includes `PAYMENT-REQUIRED`
containing a base64-encoded x402 v2 challenge. Sites with Cashu also include
`X-Cashu`. A client selects one method per request; providing both payment headers
is rejected before either payment is processed.

A Lightning client validates and pays the challenge's invoice, then repeats the
same request with an x402 v2 payload in `PAYMENT-SIGNATURE`. Its payload contains
the 32-byte payment preimage as lowercase hex. Geata checks the invoice signature,
receiver, amount, network, expiry, request binding, and preimage locally. It
records the payment hash durably before forwarding or serving the request.
Successful admission adds `PAYMENT-RESPONSE` with the payment hash and network.
No external facilitator or node lookup is needed to settle a proof.

An explicit payment always selects paid admission, even if the free allowance
has refilled. A paid request buys **one attempt**, including backend errors or a
crash after admission. There is no automatic refund. Invalid or reused proofs
return 400; storage and receiver failures return 503. A receiver outage still
allows Cashu challenges and valid previously issued Lightning proofs.

Invoice creation has a separate per-IP limit of one per second with a burst of
three, shared across sites. If issuance is unavailable or the request cannot be
bound, a dual-payment site offers Cashu alone; a Lightning-only site returns 503.
Paid retries do not create replacement invoices. Payment headers are stripped
before forwarding, and payment-enabled responses carry `Cache-Control: no-store`.

## Request binding and deployment requirements

This implements the `http:1` profile of
[x402 exact Lightning](https://github.com/x402-foundation/x402/blob/276e7cdce1620142139bf1aa6995fdad6a76cd88/specs/schemes/exact/scheme_exact_lnbtc.md).
The signed description hash binds the method, complete URL with query order
preserved, body bytes, and configured headers. A retry must preserve those values.
Unused invoices for identical requests and terms are accepted without maintaining
a challenge database. Settlement allows the specification's 60-second clock-skew
grace; newly issued invoices must be unexpired.

`lightning_headers` is required and must list **every** header affecting the operation,
content interpretation, or account selection, in lowercase sorted order without
duplicates. Include application-specific headers as needed. `lightning_headers none` is only
appropriate when no header changes these decisions. Authentication and authorization
must still run in your backend on every request. Routes whose behavior depends
on unbound context, such as client IP or client certificates, are unsupported.
MCP tool calls require a different binding profile and are not supported here.

Lightning challenge and paid-request bodies are buffered with a 64 KiB limit and
a 10-second read timeout. Upgrades and declared trailers are unsupported. Free
requests and Cashu requests retain their existing streaming behavior.

Use a receiver node key whose invoice issuance is exclusive to this service.
Do not share its invoice API with untrusted tenants. All Geata sites using a
receiver share one replay journal. Independent Geata instances must not settle
for that same receiver: this version has no shared multi-instance replay store.

## Storage and testing

The private replay database is `<data-dir>/lightning/settlements.redb`, separate
from Cashu storage. Back it up with the rest of Geata's data while stopped.
Never delete it to reset limits. Used payment hashes are retained indefinitely,
including across config reloads and restarts. A lost journal loses replay protection.
LDK Server's own backups and channel operations remain separate responsibilities.

`cargo test --test lightning` exercises the binary against a local TLS/gRPC
receiver fixture with signed invoices and HMAC authentication. Unit tests check
the specification's HTTP binding vectors, proof validation, concurrent settlement,
and restart persistence. These tests do not operate a live Lightning channel.

Back to [Geata](../README.md).
