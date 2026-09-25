# Geata

> **Repository note:** The main repository is [git.cashu.dev/thesimplekid/geata](https://git.cashu.dev/thesimplekid/geata).
> [github.com/thesimplekid/geata](https://github.com/thesimplekid/geata) is a mirror.

Geata (Irish for “gate”) is a Rust reverse proxy that lets clients **pay for
requests beyond a free rate limit using Cashu or Lightning (L402 and x402)**. It combines Caddy-style
configuration and automatic HTTPS with payment handling at the proxy.

Each client IP gets a free request allowance. When it runs out, Geata returns
**402 Payment Required** with the price and configured payment options. The client
can wait for the allowance to refill, or pay with a Cashu token or a Lightning
invoice and retry with the payment proof. Geata verifies and settles the payment
before forwarding the request, so your backend does not need payment logic.

```caddyfile
lightning my_node {
    endpoint https://localhost:3536
    api_key_file /var/lib/ldk-server/bitcoin/api_key
    tls_cert_file /var/lib/ldk-server/tls.crt
    network mainnet
    pay_to YOUR_66_CHARACTER_COMPRESSED_NODE_PUBLIC_KEY
}

example.com {
    rate_limit 10/s burst 20
    pay_over_limit 2 sat https://mint.example.com
    lightning_over_limit 2 sat my_node
    lightning_protocols x402 l402
    lightning_headers accept content-encoding content-type cookie range
    reverse_proxy localhost:3000
}
```

Here, each IP gets a bucket of 20 free requests that refills at 10 per second.
Beyond that allowance, clients can pay with **Cashu tokens, Lightning x402, or
Lightning L402**. Each method costs a fixed **2 sats** per request attempt;
Cashu also requires mint input fees, and Lightning routing fees may apply.
The price does not rise with traffic.

Try the [hosted example endpoint](https://pay.geata.thesimplekid.dev), or use the
[Rust client example](examples/README.md) to walk through a Lightning payment.

The Lightning receiver requires a separate LDK Server with incoming liquidity.
Replace its endpoint, credential paths, and node public key with your own values.
See [Lightning payments](docs/lightning.md) for setup and
[Cashu payments](docs/cashu.md) for token requirements and recovery.

Built on Pingora, Geata can also serve plain-text responses directly.

An early implementation for a single Linux server; see [current scope](docs/operations.md#current-scope).

## Payment options

Cashu and Lightning can be offered together or on their own:

| Payment method | Receiver | Where proceeds go |
| --- | --- | --- |
| Cashu tokens | Cashu mint | Geata's Cashu wallet |
| Lightning L402 | LDK Server or Cashu mint | Your Lightning node or Geata's Cashu wallet |
| Lightning x402 | LDK Server | Your Lightning node |

For Lightning, define a named receiver and reference it with
`lightning_over_limit`. Select `lightning_protocols x402 l402` to offer both
protocols through LDK Server; the default is `x402`. To accept Lightning without
running your own node, use a Cashu mint receiver with `lightning_protocols l402`.

Lightning payments are bound to the request and buy one request attempt.
Enabling L402 reserves the `Authorization` header for payment credentials,
including on free requests. See [Lightning payments](docs/lightning.md) for
complete configurations, client payment flows, and backend authentication options.
To try either protocol against a URL, run the [Rust client example](examples/README.md).

## Try it locally

Build with Nix and run the included Hello world example—no backend required:

```sh
nix build path:.
./result/bin/geata run --config examples/Geatafile.local \
  --http-listen 127.0.0.1:8080 --https-listen 127.0.0.1:8443
```

In another terminal, run `curl http://localhost:8080`.
[Other build options](docs/getting-started.md) include Cargo without Nix.

## Use your domain

Replace the example domain, mint URL, backend address, and LDK receiver settings
above with your own, then save the configuration as `Geatafile`. Point your
domain's DNS at the server and make ports 80 and 443 reachable. Then run:

```sh
./result/bin/geata validate
./result/bin/geata run
```

The process needs permission to bind those ports. Geata obtains and renews
certificates automatically, redirects HTTP to HTTPS, and reloads valid config
changes. See [HTTPS and operation](docs/operations.md) for deployment details.

## Traffic demo

```sh
nix develop path:. -c just demo
```

Open **http://127.0.0.1:9090** to watch request rates, generate traffic, and try
Cashu payments. The [demo guide](examples/traffic-demo/README.md) explains how to
choose a test or real mint and set the price.

## Documentation

- [Build and run](docs/getting-started.md)
- [Configuration](docs/configuration.md) — proxying, direct responses, reloads
- [Rate limiting](docs/rate-limiting.md) — per-IP allowances and burst limits
- [Lightning payments](docs/lightning.md) — x402 with LDK Server, L402 with LDK or a Cashu mint, request binding, replay protection
- [Cashu payments](docs/cashu.md) — pricing, tokens, wallets, recovery
- [HTTPS and operation](docs/operations.md) — certificates, storage, deployment limits
- [Development](DEVELOPMENT.md) — contributor workflow and tests
