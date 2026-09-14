# Operator payouts

Geata can pay collected sats to a Cashu payment request, either manually or
while the proxy runs. Funds stay at their original mint; this sends Cashu ecash,
not a Lightning withdrawal or a transfer between mints.

## Automatic payouts

Add a top-level block to your `Geatafile`, alongside your sites:

```caddyfile
cashu_payout {
    mint https://testnut.cashudevkit.org
    request "creqA..."
    amount 5000
    max_fee 100
    threshold 10000
    interval 1h
}
```

Replace `creqA...` with a **reusable payment request from your receiving wallet**.
Use the same mint that collects your site's payments. The test mint above uses
test funds; substitute your real mint when ready.

| Setting | Meaning |
| --- | --- |
| `mint` | Wallet whose collected sats will fund the payout. |
| `request` | Cashu payment request containing a delivery destination. |
| `amount` | Fixed amount the recipient receives in sats. Optional if embedded in the request; must match if both specify it. |
| `max_fee` | Required cap on all extra fees, including mint input fees, receiver redemption fees, and any requested payment-method fee. |
| `threshold` | Trigger when spendable balance is **at least** this many sats. |
| `interval` | Trigger after this elapsed interval; suffix `s`, `m`, `h`, or `d`, between 1 second and 365 days. |

Set `threshold`, `interval`, or both. With both, **either condition triggers** a
payout. The example sends 5,000 sats when balance reaches 10,000 sats or an hour
has passed, provided the wallet can cover the payment and fees. It debits at most
5,100 sats per payout. It does not sweep the wallet or change the payout amount
with traffic.

Schedules use elapsed time, not calendar/cron expressions. The first interval
starts when Geata first opens that payout policy's wallet; subsequent intervals
start after delivery. Timing and unresolved operations persist in redb across
restarts. A missed interval causes one payout when funds are available, without
catching up every missed interval. Mint wallets are independent: one block per
mint, up to 16 blocks.

Geata checks periodically, normally every five seconds. Mint calls can delay
checks. Every attempt has a persistent 60-second cooldown, including successful
threshold payouts; if balance remains above the threshold, further fixed payouts
can follow after that cooldown. A configured interval shorter than 60 seconds is
therefore limited by the cooldown. Payouts share the wallet lock with incoming
payments, so paid requests for that mint may briefly receive 503 and should retry
the same token. Free requests continue normally.

Edits reload with the rest of the configuration. Removing the block stops new
payouts; an operation already in progress may finish. Changing the destination
never bypasses a saved unresolved payout.

## Supported destinations

Payment requests use [NUT-18](https://cashubtc.github.io/nuts/18/). Geata supports
HTTP POST destinations and encrypted Nostr NIP-17 delivery through the request's
`nprofile` relays. CDK prefers Nostr when both are present. Public destinations
require HTTPS or WSS; plain HTTP/WS is allowed only on loopback for tests. Nostr
profiles must include 1–8 relays and the `n=17` transport tag.

Requests without a transport cannot be used for payouts; use `wallet export`
for a token you deliver yourself. Automatic payouts reject single-use requests.
The receiver must accept repeated payments using the reusable request and its
payment ID. A successful HTTP response or Nostr relay acknowledgment establishes
delivery, not that the receiving wallet has redeemed the ecash.

## Manual payment and recovery

Stop Geata before using wallet CLI commands so its data lock is free:

```sh
geata wallet --mint https://mint.example.com --data-dir /path/to/state \
    pay "$CASHU_REQUEST" --amount 5000 --max-total 5100

geata wallet --mint https://mint.example.com --data-dir /path/to/state pending

geata wallet --mint https://mint.example.com --data-dir /path/to/state \
    reclaim --operation OPERATION_ID
```

`pay` also accepts single-use requests. `--max-total` caps the entire wallet debit;
`--amount` is optional for requests that contain an amount. Manual commands send
one payment and do not install an automatic policy.

Geata saves the operation ID **before** confirming a payment. A lost response,
confirmation failure, timeout, or crash leaves the operation unresolved and
**pauses new payouts for that mint**. It never creates another payment merely
because delivery was not acknowledged. Logs identify the mint; `wallet pending`
shows the saved operation ID and unclaimed sends, including exported tokens.

`pending` runs CDK recovery and clears the pause when the operation has completed
or been safely compensated. If its ecash remains unclaimed, `reclaim` attempts to
swap it back into Geata's wallet; mint fees and spending conditions may prevent
or reduce recovery. Reclaiming invalidates those tokens for the receiver. If the
receiver already redeemed them, use `pending` to reconcile completion instead.
Restart Geata afterward. Resolved payouts retain a cooldown before automation
can send again. Never remove ledger records manually to resume payouts.

Back up the wallet seed, CDK SQLite wallet, and redb journal together while Geata
is stopped. See [Cashu storage and payment behavior](cashu.md).
