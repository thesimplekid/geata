//! Mint deposits share the same serialized wallet and recovery path as direct Cashu.
use super::*;
use cdk::{nuts::PaymentMethod, wallet::MintQuote};

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

    pub(super) async fn recover_mint_deposits(&self, mint: &MintUrl) -> anyhow::Result<()> {
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
                    if quote.amount_mintable() > Amount::ZERO {
                        stored
                            .wallet
                            .mint(&quote.id, Default::default(), None)
                            .await?;
                    }
                    Ok::<(), cdk::Error>(())
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
impl MintWallet {
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
