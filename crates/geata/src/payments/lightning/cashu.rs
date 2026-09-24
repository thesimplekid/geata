//! Durable association between an L402 invoice and a private Cashu mint quote.
use super::*;
use cdk::mint_url::MintUrl;
use redb::ReadableDatabase;

pub(super) const QUOTES: TableDefinition<&str, &str> = TableDefinition::new("l402_cashu_quotes_v1");
pub(super) const MINTS: TableDefinition<&str, bool> = TableDefinition::new("l402_cashu_mints_v1");
#[derive(Serialize, Deserialize)]
struct Deposit {
    mint: MintUrl,
    quote: String,
    amount: u64,
    invoice: String,
}

impl Lightning {
    pub(super) async fn cashu_invoice(
        &self,
        policy: &Policy,
        mint: &MintUrl,
        hash: &str,
    ) -> anyhow::Result<String> {
        let db = self.database().await?;
        let mint_url = mint.to_string();
        // Remember the mint before requesting a quote, including across cancellation.
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut tx = db.begin_write()?;
            tx.set_durability(Durability::Immediate)?;
            tx.open_table(MINTS)?.insert(mint_url.as_str(), true)?;
            tx.commit()?;
            Ok(())
        })
        .await??;
        let amount = policy.amount_msat / 1000;
        let quote = self.payments.mint_invoice(mint, amount).await?;
        let invoice = validate_invoice(policy, hash, &quote.request, now()?, 0)?;
        ensure!(
            quote.amount == Some(cdk::Amount::from(amount))
                && quote.unit == cdk::nuts::CurrencyUnit::Sat,
            "mint quote terms mismatch"
        );
        let time = now()?;
        let end = invoice
            .duration_since_epoch()
            .as_secs()
            .checked_add(invoice.expiry_time().as_secs())
            .context("invoice expiration overflow")?;
        ensure!(
            quote.expiry >= end && end <= time.saturating_add(86400),
            "mint invoice expiry must fit the quote and be within 24 hours"
        );
        let key = format!("{}:{}", policy.receiver.network(), invoice.payment_hash());
        let deposit = serde_json::to_string(&Deposit {
            mint: mint.clone(),
            quote: quote.id,
            amount,
            invoice: quote.request.clone(),
        })?;
        let db = self.database().await?;
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut tx = db.begin_write()?;
            tx.set_durability(Durability::Immediate)?;
            {
                let mut table = tx.open_table(QUOTES)?;
                ensure!(
                    table.get(key.as_str())?.is_none(),
                    "mint reused an invoice payment hash"
                );
                table.insert(key.as_str(), deposit.as_str())?;
            }
            tx.commit()?;
            Ok(())
        })
        .await??;
        Ok(quote.request)
    }

    pub(super) async fn collect_cashu_invoice(
        &self,
        policy: &Policy,
        mint: &MintUrl,
        key: &str,
    ) -> Result<(), Error> {
        let db = self.database().await.map_err(|_| Error::Unavailable)?;
        let key = key.to_owned();
        let deposit = tokio::task::spawn_blocking(move || -> anyhow::Result<Deposit> {
            let tx = db.begin_read()?;
            let table = tx.open_table(QUOTES)?;
            let record = table.get(key.as_str())?.context("missing mint deposit")?;
            Ok(serde_json::from_str(record.value())?)
        })
        .await
        .map_err(|_| Error::Unavailable)?
        .map_err(|_| Error::Unavailable)?;
        if &deposit.mint != mint || deposit.amount != policy.amount_msat / 1000 {
            return Err(Error::Invalid);
        }
        self.payments
            .collect_mint_invoice(mint, &deposit.quote, deposit.amount, &deposit.invoice)
            .await
            .map_err(|_| Error::Unavailable)
    }

    pub async fn recover_cashu_deposits(&self) -> anyhow::Result<()> {
        // Avoid creating Lightning storage on installations that never use it.
        if !self.path.try_exists()? {
            return Ok(());
        }
        let db = self.database().await?;
        let mints = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<MintUrl>> {
            let tx = db.begin_read()?;
            tx.open_table(MINTS)?
                .iter()?
                .map(|row| super::super::parse_mint(row?.0.value()))
                .collect()
        })
        .await??;
        for mint in mints {
            if self.payments.recover_mint_deposits(&mint).await.is_err() {
                // Do not log quote IDs, responses, invoices, or wallet errors.
                tracing::warn!("Cashu L402 deposit recovery deferred");
            }
        }
        Ok(())
    }
}
