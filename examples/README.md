# Rust Lightning client

From the repository root, enter `nix develop path:.`, then run:

```sh
cargo run --locked -p geata --example lightning_client -- --protocol l402 https://YOUR_DOMAIN/path
cargo run --locked -p geata --example lightning_client -- --protocol x402 https://YOUR_DOMAIN/path
```

To try the hosted example, replace `https://YOUR_DOMAIN/path` with
[`https://pay.geata.thesimplekid.dev`](https://pay.geata.thesimplekid.dev).

The [Rust example](../crates/geata/examples/lightning_client.rs) sends a GET,
prints the BOLT11 invoice from a 402 response, and prompts for the payment
preimage after you pay in an external Lightning wallet. Your wallet must expose
that preimage. It checks that the preimage matches the invoice and retries the
same GET with the selected protocol's credential. Payment is manual; the example
does not connect to a wallet or automatically spend funds.

If the endpoint still has free requests, add `--requests 25` to send up to 25
initial GETs, stopping at the first 402. Use a test endpoint: these GETs can reach
your backend. If its allowance refills faster than requests arrive, lower the
configured rate for the test. Each paid retry buys one attempt; run the example
again for a fresh invoice when testing the other protocol.

Use [Geatafile.lightning](Geatafile.lightning) for an LDK receiver with both
protocols enabled. L402 reserves `Authorization` for payment credentials; use
bound cookies or custom headers for backend authentication. For L402 through a
Cashu mint, use the configuration in the
[Lightning guide](../docs/lightning.md#receiving-l402-through-a-cashu-mint).

This example targets Geata's HTTP L402 and x402 v2 exact Lightning flows. It
supports HTTPS with system certificate verification and loopback HTTP, without
redirects, custom request headers, or request bodies. It is an interactive smoke
test, not a wallet or a complete automated invoice/request-binding validator.
The amount and network are displayed for review before you pay.

If a 402 response lacks the selected protocol's challenge header, check the
deployed server configuration. `--protocol` selects a client flow; it does not
enable that protocol on the server. `pay` alone enables Cashu tokens.
Lightning needs a receiver and `lightning_pay`; L402 also needs an explicit
`lightning_protocols l402` or `lightning_protocols x402 l402`. Mint receivers only
support L402. When Cashu is also configured, Geata can return only `X-Cashu` if
Lightning invoice creation or request binding fails, including invoice issuance
rate limits. Check receiver availability and the public origin/TLS setup as well
as the protocol settings.
