# Geata

> **Repository note:** The main repository is [git.cashu.dev/thesimplekid/geata](https://git.cashu.dev/thesimplekid/geata).
> [github.com/thesimplekid/geata](https://github.com/thesimplekid/geata) is a mirror.

Geata (Irish for “gate”) is a Rust reverse proxy that lets clients **pay for
requests beyond a free rate limit using Cashu**. It combines Caddy-style
configuration and automatic HTTPS with payment handling at the proxy.

Each client IP gets a free request allowance. When it runs out, Geata returns
**402 Payment Required** with the price and accepted Cashu mint. The client can
wait for the allowance to refill, or retry with a Cashu token. Geata verifies
and redeems the token before forwarding the request, so your backend does not
need payment logic.

```caddyfile
example.com {
    rate_limit 10/s burst 20
    pay_over_limit 2 sat https://mint.example.com
    reverse_proxy localhost:3000
}
```

Here, each IP gets a bucket of 20 free requests that refills at 10 per second.
A paid request costs a fixed **2 sats plus mint fees**; the price does not rise
with traffic. Payment buys one request attempt. See [Cashu payments](docs/cashu.md)
for token requirements, excess payments, and recovery.

Built on Pingora, Geata can also serve plain-text responses directly.

An early implementation for a single Linux server; see [current scope](docs/operations.md#current-scope).

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

Replace the example domain and mint URL above with your own, then save the
configuration as `Geatafile`. Point your domain's DNS at the server and make
ports 80 and 443 reachable. Then run:

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
- [Cashu payments](docs/cashu.md) — pricing, tokens, wallets, recovery
- [HTTPS and operation](docs/operations.md) — certificates, storage, deployment limits
- [Development](DEVELOPMENT.md) — contributor workflow and tests
