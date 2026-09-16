use super::*;
use cdk::nuts::TransportType;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayoutPolicy {
    pub mint: MintUrl,
    pub request: PaymentRequest,
    pub amount: u64,
    pub max_fee: u64,
    pub interval: Option<i64>,
    pub threshold: Option<u64>,
}

impl PayoutPolicy {
    pub fn parse(args: &[&str]) -> anyhow::Result<Self> {
        ensure!(
            args.len().is_multiple_of(2),
            "payout directives require a value"
        );
        let mut values = HashMap::new();
        for pair in args.chunks_exact(2) {
            ensure!(
                matches!(
                    pair[0],
                    "mint" | "request" | "amount" | "max_fee" | "interval" | "threshold"
                ),
                "unknown payout directive"
            );
            ensure!(
                values.insert(pair[0], pair[1]).is_none(),
                "duplicate payout directive"
            );
        }
        let mint = parse_mint(values.get("mint").context("payout requires mint")?)?;
        let amount = values
            .get("amount")
            .map(|v| v.parse::<u64>())
            .transpose()
            .context("invalid payout amount")?;
        let (request, amount) = validate_request(
            values.get("request").context("payout requires request")?,
            &mint,
            amount,
        )?;
        ensure!(
            request.single_use != Some(true),
            "automatic payout requires a reusable payment request"
        );
        let max_fee = values
            .get("max_fee")
            .context("payout requires max_fee in sats")?
            .parse::<u64>()
            .context("invalid max_fee")?;
        amount
            .checked_add(max_fee)
            .context("payout amount plus fee overflows")?;
        let interval = values
            .get("interval")
            .map(|v| parse_interval(v))
            .transpose()?;
        let threshold = values
            .get("threshold")
            .map(|v| v.parse::<u64>())
            .transpose()
            .context("invalid threshold")?;
        ensure!(threshold != Some(0), "threshold must be positive");
        ensure!(
            interval.is_some() || threshold.is_some(),
            "payout requires interval or threshold"
        );
        Ok(Self {
            mint,
            request,
            amount,
            max_fee,
            interval,
            threshold,
        })
    }

    fn due(&self, state: &PayoutState, now: i64, balance: u64) -> bool {
        state.pending.is_none()
            && now >= state.retry_at
            && balance >= self.amount
            && (self.threshold.is_some_and(|n| balance >= n)
                || self
                    .interval
                    .is_some_and(|n| now >= state.last_paid.saturating_add(n)))
    }
}

fn parse_interval(value: &str) -> anyhow::Result<i64> {
    let (digits, multiplier) = match value.as_bytes().last() {
        Some(b's') => (&value[..value.len() - 1], 1),
        Some(b'm') => (&value[..value.len() - 1], 60),
        Some(b'h') => (&value[..value.len() - 1], 3600),
        Some(b'd') => (&value[..value.len() - 1], 86400),
        _ => anyhow::bail!("interval requires s, m, h, or d suffix"),
    };
    let seconds = digits
        .parse::<i64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .context("invalid interval")?;
    ensure!(
        (1..=31_536_000).contains(&seconds),
        "interval must be between 1s and 365d"
    );
    Ok(seconds)
}

pub fn validate_request(
    encoded: &str,
    mint: &MintUrl,
    amount: Option<u64>,
) -> anyhow::Result<(PaymentRequest, u64)> {
    ensure!(encoded.len() <= 64 * 1024, "payment request exceeds 64 KiB");
    let request = PaymentRequest::from_str(encoded)
        .map_err(|_| anyhow::anyhow!("invalid Cashu payment request"))?;
    ensure!(
        request
            .unit
            .as_ref()
            .is_none_or(|u| *u == CurrencyUnit::Sat),
        "payout requires sat unit"
    );
    ensure!(
        request.mint_preferred == Some(true)
            || request.mints.is_empty()
            || request.mints.contains(mint),
        "payment request does not accept this mint"
    );
    ensure!(
        request.unit.is_some()
            || (request.amount.is_none() && request.supported_methods.is_empty()),
        "payment request amount or methods require a unit"
    );
    let requested = request.amount.map(u64::from);
    ensure!(
        requested.zip(amount).is_none_or(|(a, b)| a == b),
        "amount conflicts with payment request"
    );
    let amount = requested
        .or(amount)
        .filter(|n| *n > 0)
        .context("payment request requires a positive amount")?;
    // Match CDK's transport preference for HTTP destination checks.
    // CDK handles Nostr profiles and relay delivery.
    let transport = request
        .transports
        .iter()
        .find(|t| t._type == TransportType::Nostr)
        .or_else(|| {
            request
                .transports
                .iter()
                .find(|t| t._type == TransportType::HttpPost)
        })
        .context("payment request needs HTTP POST or Nostr transport")?;
    if transport._type == TransportType::HttpPost {
        validate_http_target(&transport.target)?;
    }
    Ok((request, amount))
}

fn validate_http_target(target: &str) -> anyhow::Result<()> {
    let url =
        url::Url::parse(target).map_err(|_| anyhow::anyhow!("invalid payout transport URL"))?;
    let loopback = url.host_str().is_some_and(|h| {
        h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    ensure!(
        url.host_str().is_some()
            && (url.scheme() == "https" || (url.scheme() == "http" && loopback))
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "payout transport requires a secure URL without credentials or fragment (loopback tests allowed)"
    );
    Ok(())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct PayoutState {
    pub last_paid: i64,
    pub retry_at: i64,
    pub pending: Option<String>,
}

impl Payments {
    pub async fn automatic_payout(&self, policy: &PayoutPolicy) -> anyhow::Result<()> {
        let entry = self.entry(&policy.mint).await;
        let Ok(mut guard) = entry.try_lock() else {
            return Ok(());
        };
        if guard.is_none() {
            *guard = Some(MintWallet::open(&self.root, &policy.mint).await?);
        }
        let stored = guard.as_mut().context("wallet unavailable")?;
        let now = crate::certificates::now();
        let mut state = match stored.ledger.payout().await? {
            Some(state) => state,
            None => {
                let state = PayoutState {
                    last_paid: now,
                    ..Default::default()
                };
                stored.ledger.save_payout(&state).await?;
                state
            }
        };
        if state.pending.is_some() || now < state.retry_at {
            return Ok(());
        }
        stored.recover().await?;
        let balance = u64::from(stored.wallet.total_balance().await?);
        if !policy.due(&state, now, balance) {
            return Ok(());
        }
        // Persist a cooldown before preparing funds; failed sends cannot create a hot loop.
        state.retry_at = now.saturating_add(60);
        stored.ledger.save_payout(&state).await?;
        stored
            .send_request(
                policy.request.clone(),
                policy.amount,
                policy.amount + policy.max_fee,
                state,
            )
            .await?;
        tracing::info!(mint = %policy.mint, amount = policy.amount, "automatic Cashu payout delivered");
        Ok(())
    }

    pub async fn pay(
        &self,
        mint: &MintUrl,
        encoded: &str,
        amount: Option<u64>,
        max_total: u64,
    ) -> anyhow::Result<()> {
        let (request, amount) = validate_request(encoded, mint, amount)?;
        ensure!(
            max_total >= amount,
            "max-total is below the requested amount"
        );
        let mut stored = MintWallet::open(&self.root, mint).await?;
        stored.recover().await?;
        let state = stored.ledger.payout().await?.unwrap_or_default();
        ensure!(
            state.pending.is_none(),
            "resolve the pending payout first with wallet pending/reclaim"
        );
        stored.send_request(request, amount, max_total, state).await
    }

    /// Recovery checks CDK completion/compensation before clearing our crash guard.
    pub async fn pending(
        &self,
        mint: &MintUrl,
        reclaim: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let mut stored = MintWallet::open(&self.root, mint).await?;
        stored.recover().await?;
        if let Some(id) = reclaim {
            stored
                .wallet
                .revoke_send(id.parse().context("invalid operation ID")?)
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "could not reclaim payout; run wallet pending to check its state"
                    )
                })?;
        }
        let mut state = stored.ledger.payout().await?.unwrap_or_default();
        if let Some(pending) = &state.pending {
            let id = pending
                .parse()
                .context("invalid saved payout operation ID")?;
            // Recovery only deletes a send saga after completing or compensating
            // it. Preparing alone may not have created a transaction yet.
            if stored.wallet.localstore.get_saga(&id).await?.is_none() {
                state.pending = None;
                state.last_paid = crate::certificates::now();
                state.retry_at = state.last_paid.saturating_add(60);
                stored.ledger.save_payout(&state).await?;
            }
        }
        let mut lines = Vec::new();
        if let Some(id) = state.pending {
            lines.push(format!("{id} payout unresolved; automatic payouts paused"));
        }
        for id in stored.wallet.get_pending_sends().await? {
            lines.push(format!(
                "{id} unclaimed send (reclaimable if still unspent)"
            ));
        }
        Ok(lines)
    }
}

impl MintWallet {
    async fn send_request(
        &mut self,
        request: PaymentRequest,
        amount: u64,
        max_total: u64,
        mut state: PayoutState,
    ) -> anyhow::Result<()> {
        self.needs_recovery = true;
        let prepared = self
            .wallet
            .prepare_pay_request(request, Some(Amount::from(amount)))
            .await
            .map_err(|_| {
                anyhow::anyhow!("cannot prepare payout; balance, fees, or mint may be unavailable")
            })?;
        if prepared.total_amount() > Amount::from(max_total) {
            prepared.cancel().await.map_err(|_| {
                anyhow::anyhow!("payout fee cap exceeded; reservation recovery required")
            })?;
            self.needs_recovery = false;
            anyhow::bail!("payout fee cap exceeded; no payment delivered");
        }
        let id = prepared.operation_id().to_string();
        state.pending = Some(id.clone());
        if let Err(error) = self.ledger.save_payout(&state).await {
            let _ = prepared.cancel().await;
            return Err(error);
        }
        // From here cancellation or any error leaves a durable guard. Never retry
        // a potentially delivered bearer token by creating a fresh payment.
        prepared.confirm().await.map_err(|_| {
            anyhow::anyhow!(
                "payout {id} outcome uncertain; automatic payouts paused; inspect wallet pending"
            )
        })?;
        state.pending = None;
        state.last_paid = crate::certificates::now();
        self.ledger.save_payout(&state).await?;
        self.needs_recovery = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cdk::nuts::Transport;
    use nostr_sdk::{ToBech32, nips::nip19::Nip19Profile};

    fn request() -> PaymentRequest {
        PaymentRequest::builder()
            .amount(10)
            .unit(CurrencyUnit::Sat)
            .add_mint("https://mint.example".parse().expect("mint"))
            .add_transport(Transport {
                _type: TransportType::HttpPost,
                target: "https://receiver.example/pay".into(),
                tags: vec![],
            })
            .build()
    }

    #[test]
    fn configuration_and_destination_validation() -> anyhow::Result<()> {
        let request = request();
        let config = |encoded: &str, extra: &str| {
            crate::config::Config::parse(&format!(
                "http://localhost {{ respond ok }}\ncashu_payout {{ mint https://mint.example request {encoded} max_fee 2 {extra} }}"
            ))
        };
        let encoded = request.to_string();
        assert!(config(&encoded, "interval 1h threshold 100").is_ok());
        for invalid in [
            "",
            "interval 0s",
            "interval 999999999999999d",
            "threshold 0",
            "amount 11 interval 1h",
            "interval 1h interval 2h",
            "unknown 10 interval 1h",
        ] {
            assert!(config(&encoded, invalid).is_err(), "{invalid}");
        }
        let mut single = request.clone();
        single.single_use = Some(true);
        assert!(config(&single.to_string(), "interval 1h").is_err());
        let mint = "https://mint.example".parse()?;
        let mut invalid = request.clone();
        invalid.transports.clear();
        assert!(validate_request(&invalid.to_string(), &mint, None).is_err());
        invalid = request.clone();
        invalid.unit = Some(CurrencyUnit::Msat);
        assert!(validate_request(&invalid.to_string(), &mint, None).is_err());
        assert!(validate_request(&encoded, &"https://other.example".parse()?, None).is_err());
        for target in [
            "http://receiver.example/pay",
            "https://user:password@receiver.example/pay",
            "https://receiver.example/pay#fragment",
        ] {
            invalid = request.clone();
            invalid.transports[0].target = target.into();
            assert!(validate_request(&invalid.to_string(), &mint, None).is_err());
        }
        let keys = nostr_sdk::Keys::generate();
        let profile = Nip19Profile::new(keys.public_key(), ["wss://relay.example".parse()?]);
        let mut nostr = request;
        nostr.transports = vec![Transport {
            _type: TransportType::Nostr,
            target: profile.to_bech32()?,
            tags: vec![vec!["n".into(), "17".into()]],
        }];
        assert!(validate_request(&nostr.to_string(), &mint, None).is_ok());
        nostr.transports[0].tags.clear();
        assert!(validate_request(&nostr.to_string(), &mint, None).is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn schedule_threshold_and_uncertainty_survive_restart() -> anyhow::Result<()> {
        let policy = PayoutPolicy::parse(&[
            "mint",
            "https://mint.example",
            "request",
            &request().to_string(),
            "max_fee",
            "2",
            "interval",
            "1h",
            "threshold",
            "100",
        ])?;
        let mut state = PayoutState {
            last_paid: 1000,
            ..Default::default()
        };
        assert!(!policy.due(&state, 4599, 99));
        assert!(policy.due(&state, 4600, 10));
        assert!(!policy.due(&state, 4600, 9));
        assert!(policy.due(&state, 1001, 100));
        state.retry_at = 1100;
        assert!(!policy.due(&state, 1099, 1000));
        state.pending = Some("uncertain-operation".into());
        let root = tempfile::tempdir()?;
        let path = root.path().join("ledger.redb");
        let ledger = Ledger::open(&path)?;
        ledger.save_payout(&state).await?;
        drop(ledger);
        let ledger = Ledger::open(&path)?;
        let recovered = ledger.payout().await?.context("saved payout")?;
        assert_eq!(recovered, state);
        assert!(!policy.due(&recovered, 10000, 1000));
        Ok(())
    }
}
