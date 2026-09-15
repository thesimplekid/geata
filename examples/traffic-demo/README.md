# Geata traffic demo

A local dashboard that sends real requests through Geata and shows the current
request rate, accepted requests, payment-required responses, and fixed cost.
It uses Python's standard library and a built Geata binary. No frontend build or
external JavaScript dependencies are needed.

From the repository root:

```sh
nix build path:.
nix develop path:. -c just demo
```

Open **http://127.0.0.1:9090**. Press **Start high traffic** to send unpaid requests
at a target of four times the free refill rate. Wait for the overflow indicator
to show **Unpaid overflow is returning 402**, then compare **Send unpaid request**
and **Send paid request** in the two cards below the graph. Paid requests require
a Cashu token (see below); submitting one keeps the background traffic running.
Each card keeps its latest manual result visible even as new traffic fills the
request log. You can also adjust the target rate or send an unpaid burst.

The default allowance is 5 requests/second per IP with a burst of 10;
overflow requests return 402 with a fixed price of 2 sats, plus mint fees.
Some unpaid requests still succeed under load as the free allowance refills.
Valid paid requests bypass that allowance. The graph and counters distinguish
unpaid successes, paid successes, blocked requests, and other errors.

Configure the mint, allowance, and price when starting:

```sh
just demo --mint https://testnut.cashudevkit.org --rate 5 --burst 10 --price 2
just demo --mint https://your-real-mint.example --price 10 --data-dir .state/real-mint-demo
```

The default mint is `https://testnut.cashudevkit.org`. Its `/v1/info` identifies
it as a test mint whose tokens are not backed by real sats. You can select any
mint supported by Geata; real-mint tokens spend real funds. Free traffic and
402 quotes work without contacting the mint.

To make a payment, use a Cashu wallet to obtain a `cashuB` token from the selected
mint, in sats, with DLEQ proofs and no spending conditions. Include enough value
for the configured price **and mint input fees**. Paste it into the dashboard and
press **Send paid request**. There is no automatic token minting or Lightning
invoice payment. The traffic generator never submits payments automatically.

One payment buys one request attempt. Excess value is retained; there is no
change or automatic refund. If the result is uncertain, keep and retry the same
token. The dashboard does not save tokens to browser storage or log them.
CDK stores received payment data in its private wallet database.

Geata's wallet and ledger persist in **`.state/traffic-demo`**, including after
Ctrl+C. Set `--data-dir` to choose another location. Stop the demo before managing
the wallet:

```sh
result/bin/geata wallet --mint https://testnut.cashudevkit.org --data-dir .state/traffic-demo balance
result/bin/geata wallet --mint https://testnut.cashudevkit.org --data-dir .state/traffic-demo export --amount 10
```

Keep the data directory when using real funds. The demo's temporary configuration
and logs are cleaned up on exit; wallet data is retained.

## What the numbers mean

- **Current request rate:** completed dashboard requests in the last second.
- **Free allowance:** Geata's per-IP token-bucket refill rate and burst capacity.
- **Price per paid request:** the fixed net price, excluding mint input fees.
- **Total overflow quotes:** count of 402 responses × configured price. This is
  quoted access cost, not money spent; repeated rejected requests add quotes.
- **Paid requests accepted:** payments followed by a 200 response from the demo
  backend. It is a request count, not a wallet balance or total token value.

The graph shows 30 one-second buckets, including the current partial second.
Counters cover all dashboard visitors since the demo started. The generator's
rate is a target; browser scheduling and latency can reduce actual throughput.
It pauses when its tab is hidden. Refreshing the page retains server counters;
restarting the demo resets counters, while wallet data remains.

The dashboard is separate from the limited site so it stays usable after the
free allowance runs out. Browser requests go through a local relay to Geata,
then to a “Hello world” backend. All visitors share the relay's loopback IP.
Direct requests to Geata also consume that IP's allowance, but do not appear in
the dashboard's metrics. This is an example app, not global proxy monitoring.

All listeners bind to loopback. Use the exact dashboard URL printed in the
terminal. To change ports or binary:

```sh
just demo --port 9091 --proxy-port 8081 --geata /path/to/geata
```

Run `just demo-check` to exercise the demo against `result/bin/geata`, including
real free/402 responses, metrics, configuration, payment rejection, and cleanup.
It also runs a Node.js regression check of the dashboard controls, including
keeping traffic active during a payment and preserving the comparison results.
Node.js is provided by the development shell and is only needed for this check.
Both checks also run in `nix flake check`. They do not contact a mint or spend
tokens; the UI check supplies payment responses locally.
