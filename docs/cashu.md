# Cashu payments

For a live dashboard with traffic controls, request rates, pricing, and manual
Cashu payments, see the [traffic demo](../examples/traffic-demo/README.md).
Run `nix build path:.` then `nix develop path:. -c just demo`.

To also offer Lightning invoices from LDK Server, see [Lightning payments](lightning.md).

## Configuration

Offer a fixed price for requests beyond the free allowance:

```caddyfile
example.com {
    rate_limit 10/s burst 20
    pay 2 sat https://mint.example.com
    max_inflight 128
    reverse_proxy localhost:3000
}
```

Replace the mint URL with a Cashu mint you trust. `pay` works with either
`reverse_proxy` or `respond`. Omit `rate_limit` to require payment for every request.
The price is a positive integer in sats; it does not change with traffic. Without this setting,
exhausting the allowance continues to return 429.

## Payment flow

When no free allowance is configured, or it is exhausted, Geata responds with **402 Payment Required**.
Its `X-Cashu` header contains a standard [NUT-24](https://cashubtc.github.io/nuts/24/)
payment request with the price and accepted mint. `Retry-After` gives the wait
for the free allowance to refill when one is configured; otherwise this header is
omitted and payment is required. A Cashu-aware client
can retry with a `cashuB` token in `X-Cashu`:

```sh
curl -i -H "X-Cashu: $CASHU_TOKEN" https://example.com/
```

## Token requirements and fees

Geata uses CDK 0.18 to verify and redeem the token before admitting the request.
The requested price is **net of mint input fees**, matching CDK payment-request
semantics. Clients must include enough to cover those fees. Insufficient amounts
are rejected before redemption. Excess value is retained; this version does not
return change. Tokens must be from the configured mint, denominated in sats,
include DLEQ proofs, and have no spending conditions. Tokens are limited to
16 KiB. Invalid, insufficient, forged, or already used payments return 400.

## What a payment authorizes

Supplying `X-Cashu` explicitly selects a paid request, even if the free allowance
has refilled. Omit it to use the free allowance. One payment buys one request
attempt, not a quota refill or guaranteed backend success. Paid requests do not
replenish or consume the free bucket. Geata removes payment tokens before calling
the backend and disables response caching for sites with payments enabled.
Other sites retain their existing forwarding behavior.

## Capacity limits

`max_inflight` caps simultaneous requests, including payment verification and
streaming responses. It defaults to 128 on every site. When the limit is greater
than one, a single client identity (IPv4 address or IPv6 /64 by default) can use
at most half of it, leaving capacity for other clients. At capacity, Geata
returns 503 **without redeeming a token**. A separate payment-verification
bucket defaults to 2 attempts/s with a burst of 8; exhaustion returns 429 before
redemption. See [request controls](rate-limiting.md#resource-and-payment-verification-controls)
for body and duration limits, IPv6 aggregation, and configuration overrides.
Mint operations are serialized per mint; concurrent payment attempts may also
receive 503 with `Retry-After`. Use HTTPS for public payments; HTTP mint URLs and
HTTP token submission are supported only for loopback testing.

## Wallet storage

Wallet seeds (`seed`), CDK SQLite wallets (`wallet.sqlite`), and redb payment
admission and payout journals (`ledger.redb`) live under
`<data-dir>/payments`, separated by mint. Directories are private (0700), and
files are private (0600). Back up the entire data directory while Geata is stopped;
never delete payment journals to reset request limits. Replays remain rejected
across restarts and configuration reloads. Changing a site's price or mint may
prevent retrying an earlier interrupted payment against that site.

## Interrupted payments

If the mint response is lost or Geata restarts during redemption, retry the same
token after a 503. CDK recovers the wallet operation, and Geata can admit the
request if that payment has not already authorized an attempt. Admission is
recorded before running the handler: a crash after that point, an interrupted
response, or a backend error can consume the attempt. There are no automatic
refunds or replays of backend actions.

## Operator payouts

Send collected sats to a Cashu payment request manually, on an interval, or when
the wallet reaches a balance threshold. See [operator payouts](payouts.md) for
configuration, fee caps, supported destinations, and recovery commands.

## Balance and export

To inspect or export collected funds, stop Geata first so its data lock is free:

```sh
geata wallet --mint https://mint.example.com --data-dir /path/to/state balance
geata wallet --mint https://mint.example.com --data-dir /path/to/state export --amount 100
```

`export` writes a bearer Cashu token to stdout; keep that output private. The
amount must be available in the wallet; CDK may require additional mint fees.
Restart Geata after managing the wallet.

## Examples and tests

[examples/Geatafile.cashu](../examples/Geatafile.cashu) demonstrates a local paid
`respond` site. Run it with loopback listener overrides as in the rate-limit
example. It advertises 402 payments without a running mint; actual payments need
a mint at the configured address. For an automated local test with no real money,
run `just cashu`: it starts a protocol fixture with real CDK signatures and tests
redemption, fees, replay prevention, capacity, mint outages, crash recovery, and
the wallet commands.

Back to [Geata](../README.md).
