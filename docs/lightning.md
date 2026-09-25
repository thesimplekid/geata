# Lightning payments with LDK Server or a Cashu mint

Geata can offer Lightning x402 and L402 payments alongside Cashu, or on their own.
For x402, run a separate [LDK Server](https://github.com/lightningdevkit/ldk-server) instance
with incoming Lightning liquidity. Geata creates invoices through its authenticated
TLS gRPC API. Funds stay on that node. L402 can instead receive through a Cashu
mint, keeping the proceeds in Geata's Cashu wallet without your own node.

The `ldk-server-client` adapter targets `api.LightningNode/Bolt11Receive` with the description-hash
variant of `Bolt11InvoiceDescription`, as defined at LDK Server commit
[`08316de`](https://github.com/lightningdevkit/ldk-server/tree/08316de8795a759eee08dc3873d32f980fbd0fe0).
Older versions without this API are unsupported. Upstream currently describes
LDK Server as experimental and not ready for production use.

## Configuration

Define a named receiver and reference it from each paid site in the Geatafile:

```caddyfile
lightning my_node {
    endpoint https://localhost:3536
    macaroon_file /var/lib/ldk-server/bitcoin/macaroons/geata.macaroon
    tls_cert_file /var/lib/ldk-server/tls.crt
    network mainnet
    pay_to YOUR_66_CHARACTER_COMPRESSED_NODE_PUBLIC_KEY
    expiry_seconds 300
}

example.com {
    rate_limit 10/s burst 20
    pay 2 sat https://mint.example.com
    lightning_pay 2 sat my_node
    lightning_headers accept authorization content-encoding content-type cookie range
    reverse_proxy localhost:3000
}
```

Create a dedicated token on the upgraded LDK host:

```sh
ldk create-macaroon geata --permissions invoices:create
```

Install the returned JSON `.token` as a private hex-encoded macaroon file
(root-owned, mode `0600`, passed through systemd `LoadCredential` when applicable).
Geata only needs `invoices:create`; do not give it the admin or LNURL token.
The client binds authentication to each invoice request automatically.

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

`lightning_pay <price> sat <receiver name>` requires `lightning_headers`.
Omit `rate_limit` to require payment for every request, or add it to grant a free
allowance before payment is required. Prices are positive whole sats, converted exactly to
millisatoshis on the wire. Cashu and Lightning prices may differ. Omit
`pay` for Lightning only. The default `max_inflight` is 128 on every site. Payment
verification has a separate per-client limit of 2 attempts/s with a burst of 8,
including malformed credentials. See [request controls](rate-limiting.md#resource-and-payment-verification-controls)
for overrides, IPv6 aggregation, and body and duration limits.

`lightning_headers <names...>` explicitly selects the site's bound headers in
lowercase sorted order. Use `lightning_headers none` only when no header affects
the purchased operation, content interpretation, or account selection. These
settings belong to the site, not the receiver. See request binding below.

The public origin defaults to the site's scheme and domain, such as
`https://example.com`. For a nonstandard listener port, set it on the site:

```caddyfile
http://localhost {
    rate_limit 1/s burst 1
    lightning_pay 2 sat my_node
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

## Receiving L402 through a Cashu mint

To receive L402 payments without running your own Lightning node, define a mint
receiver. The mint runs the Lightning infrastructure and holds the sats backing
your Cashu balance; choose a mint you trust.

```caddyfile
lightning my_mint {
    mint https://mint.example.com
    network mainnet
}

example.com {
    rate_limit 10/s burst 20
    pay 2 sat https://mint.example.com
    lightning_pay 2 sat my_mint
    lightning_protocols l402
    lightning_headers accept content-encoding content-type cookie range
    reverse_proxy localhost:3000
}
```

The receiver accepts only `mint` and `network` (`mainnet` or `testnet`). The mint
must support BOLT11 mint quotes denominated in sats. Public mint URLs require
HTTPS; loopback HTTP is allowed for testing. The mint chooses invoice expiry;
Geata requires it to fit within the quote expiry and the next 24 hours. No LDK
endpoint, node public key, or API key file is needed.

Mint receivers require **`lightning_protocols l402`**. Lightning x402 still needs
LDK Server because ordinary mint invoices do not support its required signed
request-description hash. L402 binds the request through its signed macaroon.
Direct Cashu and mint-backed L402 can be offered together; they share the wallet
when configured with the same mint URL. Each site currently selects one named
Lightning receiver.

Geata privately persists the mint quote and its association with the invoice
before advertising a challenge. A customer pays the Lightning invoice and sends
the normal L402 credential. Geata verifies the credential, claims the ecash into
its wallet, then durably consumes the payment for one request attempt. It requires
a completed local mint transaction; a preimage or remote payment-status response
alone does not grant admission. The quote ID and quote signing key are never
included in the public challenge.

If the mint is unavailable, settlement returns 503: retry the same credential.
A lost mint response can be recovered through CDK's persisted mint operation and
signature restoration. For reliable recovery, use a mint supporting NUT-09.
Wallet dependency logs are suppressed because they can contain private quotes
or token material; Geata reports failures without those details.

A background loop also claims paid deposits when a client never retries. It checks
up to 32 outstanding quotes per mint per pass, rotates through pending quotes,
and retains expired quotes for recovery of already-paid funds. The loop pauses
30 seconds between passes and shares the wallet lock with direct Cashu admission
and payouts. Previously used receiver mints remain recorded and checked across
restarts, even if their sites are removed, so paid deposits can still be collected.
Recovery collects funds; it does not spend the HTTP request allowance.

Proceeds use the existing [Cashu wallet and payout commands](cashu.md). Back up
**both** `<data-dir>/payments` and `<data-dir>/lightning` while Geata is stopped.
The former contains wallet seeds, quote keys, and ecash; the latter contains L402
credentials, quote associations, and replay protection. Do not delete either to
reset limits.

## L402 alongside x402 and Cashu

`lightning_protocols` selects `x402`, `l402`, or both; it defaults to `x402`.
To offer all three payment methods, use this site with the receiver above:

```caddyfile
example.com {
    rate_limit 10/s burst 20
    pay 2 sat https://mint.example.com
    lightning_pay 2 sat my_node
    lightning_protocols x402 l402
    lightning_headers accept content-encoding content-type cookie range
    reverse_proxy localhost:3000
}
```

When L402 is enabled, `Authorization` is reserved for the payment credential.
It cannot appear in `lightning_headers`; other Authorization schemes are rejected,
even on free requests. Use a bound cookie or custom header for backend authentication.
Existing x402-only sites can continue binding and forwarding backend Authorization.

An HTTP 402 response advertises L402 using:

```http
WWW-Authenticate: L402 macaroon="<base64>", invoice="<bolt11>"
```

The client pays the invoice and retries the identical request with:

```http
Authorization: L402 <base64-macaroon>:<hex-preimage>
```

With both Lightning protocols enabled, `PAYMENT-REQUIRED` contains the **same
invoice**. Redeeming either proof consumes it for both protocols. Cashu remains
a separate alternative, advertised in `X-Cashu`. Send exactly one payment method.
L402 credentials are stripped before forwarding. Invalid, expired, or reused
L402 credentials return 401 without issuing another invoice; storage failures
return 503. Successful L402 requests do not add an x402 `PAYMENT-RESPONSE`.

This implements HTTP [L402](https://github.com/lightninglabs/L402/blob/master/protocol-specification.md)
using V2 macaroons with V0 identifiers and per-credential random root keys.
Each credential buys one request attempt, not a reusable subscription or quota.
The signed first-party caveats bind the `geata:0` service, request hash, current
payment terms, and expiry (including the same 60-second grace as x402).
The supported predicates are `services`, `geata_request`, `geata_terms`,
`geata_valid_until` (exclusive Unix seconds), and optional `preimage`.
Clients can append matching predicates or shorten the expiry; unknown or
unsatisfied caveats, third-party caveats, and macaroon bundles are rejected.
The gRPC L402 profile is not supported.

## Payment flow

With x402 enabled, when there is no free allowance or it runs out, a 402 response includes `PAYMENT-REQUIRED`
containing a base64-encoded x402 v2 challenge. Sites with Cashu also include
`X-Cashu`. A client selects one method per request; providing both payment headers
is rejected before either payment is processed.

A Lightning client validates and pays the challenge's invoice, then repeats the
same request with an x402 v2 payload in `PAYMENT-SIGNATURE`. Its payload contains
the 32-byte payment preimage as lowercase hex. Geata checks the invoice signature,
receiver, amount, network, expiry, request binding, and preimage locally. It
records the payment hash durably before forwarding or serving the request.
Successful admission adds `PAYMENT-RESPONSE` with the payment hash and network.
LDK-backed proofs need no external facilitator or node lookup for settlement.
Mint-backed L402 also needs confirmation that ecash has reached the local wallet.

An explicit payment always selects paid admission, even if the free allowance
has refilled. A paid request buys **one attempt**, including backend errors or a
crash after admission. There is no automatic refund. Invalid or reused x402 proofs
return 400 (L402 returns 401); storage and receiver failures return 503. An LDK receiver outage still
allows Cashu challenges and valid previously issued LDK-backed Lightning proofs.
Mint-backed L402 requires the mint until its ecash has been received.

Invoice creation has a separate per-IP limit of one per second with a burst of
three, shared across sites. If issuance is unavailable or the request cannot be
bound, a dual-payment site offers Cashu alone; a Lightning-only site returns 503.
Paid retries do not create replacement invoices. Payment headers are stripped
before forwarding, and payment-enabled responses carry `Cache-Control: no-store`.

## Request binding and deployment requirements

Sites without bound headers use the `http:1` profile of
[x402 exact Lightning](https://github.com/x402-foundation/x402/blob/276e7cdce1620142139bf1aa6995fdad6a76cd88/specs/schemes/exact/scheme_exact_lnbtc.md).
Sites with bound headers advertise Geata's `geata:http:2` profile. Each header's
`valueHash` hashes the compact JSON array of its ASCII values, in occurrence
order, without trimming. Missing headers encode as `[]`; an empty occurrence
encodes as `[""]`. Duplicate fields differ from a single comma-containing field.
The binding domain is `x402:exact:lnbtc:bolt11:geata:http:2`; the rest of the
binding object is unchanged. Clients must support this profile and obtain fresh
challenges after upgrading; credentials bound with the former encoding are not
accepted for sites with bound headers.
The signed description hash binds the method, complete URL with query order
preserved, body bytes, and configured headers. A retry must preserve those values.
Unused invoices for identical requests and terms are accepted without maintaining
a challenge database for x402. L402 also requires its stored credential root key. Settlement allows the specification's 60-second clock-skew
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

For LDK receivers, use a node key whose invoice issuance is exclusive to this service.
Do not share its invoice API with untrusted tenants. All Geata sites using a
receiver share one replay journal. Independent Geata instances must not settle
for that same receiver: this version has no shared multi-instance replay store.

## Storage and testing

The private replay database is `<data-dir>/lightning/settlements.redb`, separate
from Cashu storage. Back it up with the rest of Geata's data while stopped.
Never delete it to reset limits. Used payment hashes and private L402 root keys
survive configuration reloads and restarts until their retention deadlines.
Losing this database invalidates outstanding L402 credentials and loses x402
replay protection. Protect backups as payment secrets.
LDK Server's own backups and channel operations remain separate responsibilities.

`cargo test --test lightning` exercises the binary against a local TLS/gRPC
receiver fixture with signed invoices and request-bound macaroon authentication. Unit tests check
the specification's HTTP binding vectors, proof validation, concurrent settlement,
and restart persistence. `cargo test --test cashu` also exercises mint-backed L402,
lost mint responses, deposit recovery, and a shared Cashu balance. These tests do
not operate a live Lightning channel.


## Payment record retention

Expired L402 roots and replay barriers are pruned when the payment database
opens and every 30 seconds, independently of mint recovery. Root expiration is
exclusive; replay barriers retain the existing clock-skew and one-hour safety
window. Cleanup never evicts live credentials or live replay barriers.

At most 10,000 outstanding records (L402 roots plus Cashu quote associations)
are allowed per process database. Cashu L402 issuance needs two slots. Issuance
fails closed at capacity, while existing payments can still settle. Each mint
wallet also caps stored quotes at 10,000, including quotes left by cancelled or
failed issuance. These limits survive restarts through the stored record counts.

Invoice RPCs allow up to 8 concurrent requests per receiver endpoint or mint,
with at most 2 from one client identity. Across receivers, the process allows
64 concurrent requests and at most 8 per client. IPv6 identities use /64
prefixes. Saturated budgets reject immediately without queuing. Receiver aliases
and credential rotations share the endpoint's budget, which is released on
completion or cancellation. Cashu wallet mutations retain their per-mint
serialization; a busy mint does not hold a process-wide issuance lock. Database
insertion still checks the durable record cap atomically.

Cashu recovery runs at startup and every 30 seconds after each pass. Cleanup
rotates through at most 32 associations per pass. Once the invoice's validity,
clock-skew grace, and one-hour retention window have elapsed, a quote is removed
when durable wallet history proves collection, or a fresh mint status response
confirms it is unpaid with zero paid/issued amounts and matching quote terms.
Unpaid cleanup also requires the mint quote's own expiry plus the same grace
window to have elapsed. Paid, pending, mismatched, unreachable, or otherwise
uncertain records remain recoverable and count against capacity. Retirement is
journaled before deleting the wallet quote and association, so a restart can
finish interrupted cleanup. Orphan wallet quotes from cancelled issuance are
also checked during recovery. Financial transaction history is retained.

Pruning permits database pages to be reused; it does not promise to shrink the
files immediately. Back up payment storage before any offline compaction.

Back to [Geata](../README.md).
