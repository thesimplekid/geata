//! Mint deposits share the same serialized wallet and recovery path as direct Cashu.
use super::*;
use cdk::{
    MintQuoteResponse,
    nuts::{MintQuoteState, PaymentMethod},
    wallet::MintQuote,
};

impl Payments {
    pub(super) async fn mint_invoice(
        &self,
        mint: &MintUrl,
        amount: u64,
    ) -> anyhow::Result<MintQuote> {
        tokio::time::timeout(PAYMENT_TIMEOUT, async {
            let entry = self.entry(mint).await;
            let mut guard = entry.try_lock().context("mint wallet busy")?;
            if guard.is_none() {
                *guard = Some(MintWallet::open(&self.root, mint).await?);
            }
            let stored = guard.as_mut().context("wallet unavailable")?;
            stored.recover().await?;
            // Includes quotes created before cancellation or validation failures,
            // which may not yet have a Lightning association record.
            ensure!(
                stored.wallet.localstore.get_mint_quotes().await?.len()
                    < lightning::MAX_CHALLENGE_RECORDS as usize,
                "mint quote capacity exhausted"
            );
            Ok(stored
                .wallet
                .mint_quote(
                    PaymentMethod::BOLT11,
                    Some(Amount::from(amount)),
                    None,
                    None,
                )
                .await?)
        })
        .await?
    }

    pub(super) async fn collect_mint_invoice(
        &self,
        mint: &MintUrl,
        id: &str,
        amount: u64,
        invoice: &str,
    ) -> anyhow::Result<()> {
        tokio::time::timeout(PAYMENT_TIMEOUT, async {
            let entry = self.entry(mint).await;
            let mut guard = entry.try_lock().context("mint wallet busy")?;
            if guard.is_none() {
                *guard = Some(MintWallet::open(&self.root, mint).await?);
            }
            let stored = guard.as_mut().context("wallet unavailable")?;
            stored.recover().await?;
            if stored.has_mint_transaction(id, amount, invoice).await? {
                return Ok(());
            }
            let quote = stored
                .wallet
                .localstore
                .get_mint_quote(id)
                .await?
                .context("missing mint quote")?;
            ensure!(
                quote.request == invoice
                    && quote.amount == Some(Amount::from(amount))
                    && quote.unit == CurrencyUnit::Sat,
                "mint quote mismatch"
            );
            // CDK persists outputs and its saga before sending the mint request.
            // Retain recovery on cancellation or a lost mint response.
            stored.needs_recovery = true;
            let quote = stored.wallet.check_mint_quote_status(id).await?;
            if quote.amount_mintable() > Amount::ZERO {
                stored.wallet.mint(id, Default::default(), None).await?;
            }
            ensure!(
                stored.has_mint_transaction(id, amount, invoice).await?,
                "mint funds not yet received"
            );
            stored.needs_recovery = false;
            Ok(())
        })
        .await?
    }

    // Caller journals retirement before removing either copy of the quote.
    pub(super) async fn can_prune_mint_deposit(
        &self,
        mint: &MintUrl,
        id: &str,
        amount: u64,
        invoice: &str,
    ) -> anyhow::Result<bool> {
        tokio::time::timeout(PAYMENT_TIMEOUT, async {
            let entry = self.entry(mint).await;
            let mut guard = entry.try_lock().context("mint wallet busy")?;
            if guard.is_none() {
                *guard = Some(MintWallet::open(&self.root, mint).await?);
            }
            let stored = guard.as_mut().context("wallet unavailable")?;
            stored.recover().await?;
            if stored.has_mint_transaction(id, amount, invoice).await? {
                return Ok(true);
            }
            let Some(quote) = stored.wallet.localstore.get_mint_quote(id).await? else {
                return Ok(false);
            };
            if quote.request != invoice || quote.amount != Some(Amount::from(amount)) {
                return Ok(false);
            }
            stored.definitively_unpaid(&quote).await
        })
        .await?
    }

    pub(super) async fn remove_retired_mint_quote(
        &self,
        mint: &MintUrl,
        id: &str,
    ) -> anyhow::Result<()> {
        tokio::time::timeout(PAYMENT_TIMEOUT, async {
            let entry = self.entry(mint).await;
            let mut guard = entry.try_lock().context("mint wallet busy")?;
            if guard.is_none() {
                *guard = Some(MintWallet::open(&self.root, mint).await?);
            }
            let stored = guard.as_mut().context("wallet unavailable")?;
            stored.recover().await?;
            stored.wallet.localstore.remove_mint_quote(id).await?;
            Ok(())
        })
        .await?
    }

    pub(super) async fn recover_mint_deposits(
        &self,
        mint: &MintUrl,
        associated: &std::collections::HashSet<String>,
    ) -> anyhow::Result<()> {
        tokio::time::timeout(PAYMENT_TIMEOUT, async {
            let entry = self.entry(mint).await;
            let mut guard = entry.try_lock().context("mint wallet busy")?;
            if guard.is_none() {
                *guard = Some(MintWallet::open(&self.root, mint).await?);
            }
            let stored = guard.as_mut().context("wallet unavailable")?;
            stored.recover().await?;
            // Also claim paid deposits whose client never retried the HTTP request.
            // Do not filter expired invoices: expiration must not discard paid funds.
            let mut quotes = stored.wallet.get_unissued_mint_quotes().await?;
            quotes.sort_by(|a, b| a.id.cmp(&b.id));
            let start = stored
                .deposit_cursor
                .as_ref()
                .map_or(0, |cursor| quotes.partition_point(|q| q.id <= *cursor));
            let count = quotes.len();
            if count > 0 {
                quotes.rotate_left(start % count);
            }
            // Bound each pass and rotate so old unpaid or failing quotes cannot
            // permanently starve later deposits. Advance before a possible timeout.
            for quote in quotes.into_iter().take(32) {
                stored.deposit_cursor = Some(quote.id.clone());
                stored.needs_recovery = true;
                let result = async {
                    let quote = stored.wallet.check_mint_quote_status(&quote.id).await?;
                    if !associated.contains(&quote.id) && stored.definitively_unpaid(&quote).await?
                    {
                        stored
                            .wallet
                            .localstore
                            .remove_mint_quote(&quote.id)
                            .await?;
                    } else if quote.amount_mintable() > Amount::ZERO {
                        stored
                            .wallet
                            .mint(&quote.id, Default::default(), None)
                            .await?;
                    }
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                if result.is_err() {
                    stored.recover().await?;
                    tracing::warn!("Cashu deposit could not yet be claimed");
                } else {
                    stored.needs_recovery = false;
                }
            }
            Ok(())
        })
        .await?
    }
}
fn past_quote_grace(expiry: u64, time: u64) -> bool {
    expiry != 0
        && expiry
            .checked_add(lightning::QUOTE_PRUNE_GRACE)
            .is_some_and(|end| time >= end)
}

impl MintWallet {
    async fn definitively_unpaid(&self, quote: &MintQuote) -> anyhow::Result<bool> {
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        if quote.state != MintQuoteState::Unpaid
            || quote.amount_paid != Amount::ZERO
            || quote.amount_issued != Amount::ZERO
            || quote.used_by_operation.is_some()
            || quote.payment_method != PaymentMethod::BOLT11
            || quote.unit != CurrencyUnit::Sat
            || !past_quote_grace(quote.expiry, time)
        {
            return Ok(false);
        }
        let Ok(invoice) = quote.request.parse::<lightning_invoice::Bolt11Invoice>() else {
            return Ok(false);
        };
        let end = invoice
            .duration_since_epoch()
            .as_secs()
            .saturating_add(invoice.expiry_time().as_secs());
        if !past_quote_grace(end, time) {
            return Ok(false);
        }
        // Inspect the fresh response, not CDK's cached/merged state: invalid or
        // stale accounting responses must not be mistaken for an unpaid quote.
        let response = self
            .wallet
            .mint_connector()
            .get_mint_quote_status(PaymentMethod::BOLT11, &quote.id)
            .await?;
        let MintQuoteResponse::Bolt11(response) = response else {
            return Ok(false);
        };
        Ok(response.quote == quote.id
            && response.request == quote.request
            && response.amount == quote.amount
            && response.unit == Some(quote.unit.clone())
            && response.method == PaymentMethod::BOLT11
            && response.state == MintQuoteState::Unpaid
            && response.amount_paid == Amount::ZERO
            && response.amount_issued == Amount::ZERO
            && response.updated_at >= quote.updated_at
            && response
                .expiry
                .is_some_and(|expiry| past_quote_grace(expiry, time)))
    }

    async fn has_mint_transaction(
        &self,
        id: &str,
        amount: u64,
        invoice: &str,
    ) -> anyhow::Result<bool> {
        Ok(self
            .wallet
            .list_transactions(Some(TransactionDirection::Incoming))
            .await?
            .iter()
            .any(|tx| {
                tx.quote_id.as_deref() == Some(id)
                    && tx.payment_request.as_deref() == Some(invoice)
                    && tx.status == TransactionStatus::Completed
                    && tx.unit == CurrencyUnit::Sat
                    && tx.amount >= Amount::from(amount)
            }))
    }
}

#[cfg(test)]
pub(super) struct QuoteStatusFixture {
    pub mint: MintUrl,
    pub response: Arc<parking_lot::Mutex<(u16, serde_json::Value)>>,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(test)]
impl Drop for QuoteStatusFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
impl QuoteStatusFixture {
    pub async fn new(invoice: &str, id: &str) -> anyhow::Result<Self> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let mint = parse_mint(&format!("http://{}", listener.local_addr()?))?;
        let response = Arc::new(parking_lot::Mutex::new((
            200,
            serde_json::json!({
                "quote": id, "request": invoice, "amount": 25, "unit": "sat",
                "state": "UNPAID", "expiry": 1, "amount_paid": 0, "amount_issued": 0,
            }),
        )));
        let shared = response.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < 8192 {
                    let Ok(n) = stream.read(&mut buf).await else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                }
                let (status, value) = shared.lock().clone();
                let body = value.to_string();
                let reply = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            }
        });
        Ok(Self {
            mint,
            response,
            task,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cdk::{nuts::MintQuoteState, wallet::types::Transaction};

    #[tokio::test]
    async fn expired_unpaid_quotes_require_fresh_matching_mint_confirmation() -> anyhow::Result<()>
    {
        let root = tempfile::tempdir()?;
        let payments = Payments::new(root.path());
        let p = lightning::tests::policy();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let invoice = lightning::tests::signed(&p, &"00".repeat(32), now - 7200);
        let fixture = QuoteStatusFixture::new(&invoice, "unpaid").await?;
        let entry = payments.entry(&fixture.mint).await;
        let mut stored = MintWallet::open(&payments.root, &fixture.mint).await?;
        stored.recover().await?;
        let store = stored.wallet.localstore.clone();
        let quote = MintQuote::new(
            "unpaid".into(),
            fixture.mint.clone(),
            PaymentMethod::BOLT11,
            Some(Amount::from(25)),
            CurrencyUnit::Sat,
            invoice.clone(),
            1,
            None,
        );
        store.add_mint_quote(quote.clone()).await?;
        *entry.lock().await = Some(stored);
        let valid = fixture.response.lock().1.clone();
        for state in ["PAID", "ISSUED", "PENDING"] {
            fixture.response.lock().1["state"] = serde_json::json!(state);
            assert!(
                !payments
                    .can_prune_mint_deposit(&fixture.mint, "unpaid", 25, &invoice)
                    .await
                    .unwrap_or(false)
            );
            assert!(store.get_mint_quote("unpaid").await?.is_some());
        }
        *fixture.response.lock() = (503, serde_json::json!({"error": "offline"}));
        assert!(
            payments
                .can_prune_mint_deposit(&fixture.mint, "unpaid", 25, &invoice)
                .await
                .is_err()
        );
        for (field, value) in [
            ("amount_paid", serde_json::json!(1)),
            ("quote", serde_json::json!("other")),
            ("expiry", serde_json::json!(now + 3600)),
        ] {
            *fixture.response.lock() = (200, valid.clone());
            fixture.response.lock().1[field] = value;
            assert!(
                !payments
                    .can_prune_mint_deposit(&fixture.mint, "unpaid", 25, &invoice)
                    .await?
            );
        }
        *fixture.response.lock() = (200, valid);
        let mut recent = quote.clone();
        recent.expiry = now;
        store.add_mint_quote(recent).await?;
        assert!(
            !payments
                .can_prune_mint_deposit(&fixture.mint, "unpaid", 25, &invoice)
                .await?
        );
        let mut expired = store.get_mint_quote("unpaid").await?.expect("quote");
        expired.expiry = 1;
        store.add_mint_quote(expired).await?;
        assert!(
            payments
                .can_prune_mint_deposit(&fixture.mint, "unpaid", 25, &invoice)
                .await?
        );
        // Status inspection itself does not delete anything before journaling.
        assert!(store.get_mint_quote("unpaid").await?.is_some());
        payments
            .recover_mint_deposits(&fixture.mint, &Default::default())
            .await?;
        assert!(
            store.get_mint_quote("unpaid").await?.is_none(),
            "orphan quote retained quota"
        );
        Ok(())
    }

    #[tokio::test]
    async fn pruning_retains_uncertain_funds_and_is_idempotent_after_collection()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let payments = Payments::new(root.path());
        let mint = parse_mint("https://mint.example.com")?;
        let entry = payments.entry(&mint).await;
        let mut stored = MintWallet::open(&payments.root, &mint).await?;
        stored.recover().await?;
        let store = stored.wallet.localstore.clone();
        let quote = MintQuote {
            id: "deposit".into(),
            mint_url: mint.clone(),
            payment_method: PaymentMethod::BOLT11,
            amount: Some(Amount::from(25)),
            unit: CurrencyUnit::Sat,
            request: "invoice".into(),
            state: MintQuoteState::Unpaid,
            expiry: 1,
            secret_key: None,
            amount_issued: Amount::ZERO,
            amount_paid: Amount::ZERO,
            updated_at: 0,
            estimated_blocks: None,
            used_by_operation: None,
            version: 0,
        };
        store.add_mint_quote(quote.clone()).await?;
        *entry.lock().await = Some(stored);
        assert!(
            !payments
                .can_prune_mint_deposit(&mint, "deposit", 25, "invoice")
                .await?
        );
        assert!(store.get_mint_quote("deposit").await?.is_some());
        let mut paid = quote;
        paid.state = MintQuoteState::Paid;
        paid.amount_paid = Amount::from(25);
        store.add_mint_quote(paid).await?;
        assert!(
            !payments
                .can_prune_mint_deposit(&mint, "deposit", 25, "invoice")
                .await?
        );
        store
            .add_transaction(Transaction {
                mint_url: mint.clone(),
                direction: TransactionDirection::Incoming,
                amount: Amount::from(25),
                fee: Amount::ZERO,
                unit: CurrencyUnit::Sat,
                ys: vec![],
                timestamp: 1,
                memo: None,
                metadata: Default::default(),
                quote_id: Some("deposit".into()),
                payment_request: Some("invoice".into()),
                payment_proof: None,
                payment_method: Some(PaymentMethod::BOLT11),
                saga_id: None,
                status: TransactionStatus::Completed,
            })
            .await?;
        assert!(
            !payments
                .can_prune_mint_deposit(&mint, "deposit", 26, "invoice")
                .await?
        );
        assert!(
            !payments
                .can_prune_mint_deposit(&mint, "deposit", 25, "other invoice")
                .await?
        );
        assert!(
            payments
                .can_prune_mint_deposit(&mint, "deposit", 25, "invoice")
                .await?
        );
        assert!(store.get_mint_quote("deposit").await?.is_some());
        payments.remove_retired_mint_quote(&mint, "deposit").await?;
        payments.remove_retired_mint_quote(&mint, "deposit").await?;
        assert!(store.get_mint_quote("deposit").await?.is_none());
        assert!(
            payments
                .can_prune_mint_deposit(&mint, "deposit", 25, "invoice")
                .await?
        );
        assert_eq!(store.list_transactions(None, None, None).await?.len(), 1);
        Ok(())
    }
}
