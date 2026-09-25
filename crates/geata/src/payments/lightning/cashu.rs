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
    #[serde(default)]
    retired: bool,
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
            retired: false,
        })?;
        let db = self.database().await?;
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut tx = db.begin_write()?;
            tx.set_durability(Durability::Immediate)?;
            {
                super::maintenance::check_capacity(&tx, 1)?;
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

    async fn prune_cashu_deposits(&self) -> anyhow::Result<()> {
        let mut cursor = self.cashu_prune_cursor.try_lock().context("cleanup busy")?;
        let db = self.database().await?;
        let read_db = db.clone();
        let mut deposits =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(String, Deposit)>> {
                let tx = read_db.begin_read()?;
                let table = tx.open_table(QUOTES)?;
                table
                    .iter()?
                    .map(|row| {
                        let (key, value) = row?;
                        Ok((key.value().to_owned(), serde_json::from_str(value.value())?))
                    })
                    .collect()
            })
            .await??;
        let start = cursor.as_ref().map_or(0, |cursor| {
            deposits.partition_point(|(key, _)| key <= cursor)
        });
        if !deposits.is_empty() {
            let count = deposits.len();
            deposits.rotate_left(start % count);
        }
        let time = now()?;
        for (key, mut deposit) in deposits.into_iter().take(32) {
            *cursor = Some(key.clone());
            let invoice: Bolt11Invoice = deposit.invoice.parse()?;
            let retain_until = invoice
                .duration_since_epoch()
                .as_secs()
                .saturating_add(invoice.expiry_time().as_secs())
                .saturating_add(SKEW + 3600);
            if time < retain_until {
                continue;
            }
            if !deposit.retired {
                match self
                    .payments
                    .can_prune_mint_deposit(
                        &deposit.mint,
                        &deposit.quote,
                        deposit.amount,
                        &deposit.invoice,
                    )
                    .await
                {
                    Ok(true) => {}
                    Ok(false) | Err(_) => continue,
                }
                // Journal the decision before deleting the CDK record. After a
                // crash, resume deletion without mistaking a missing local quote
                // for an unreachable mint or losing an association quota slot.
                deposit.retired = true;
                let value = serde_json::to_string(&deposit)?;
                let db = db.clone();
                let key = key.clone();
                tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    let mut tx = db.begin_write()?;
                    tx.set_durability(Durability::Immediate)?;
                    tx.open_table(QUOTES)?
                        .insert(key.as_str(), value.as_str())?;
                    tx.commit()?;
                    Ok(())
                })
                .await??;
            }
            if self
                .payments
                .remove_retired_mint_quote(&deposit.mint, &deposit.quote)
                .await
                .is_err()
            {
                continue;
            }
            let db = db.clone();
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let mut tx = db.begin_write()?;
                tx.set_durability(Durability::Immediate)?;
                tx.open_table(QUOTES)?.remove(key.as_str())?;
                tx.commit()?;
                Ok(())
            })
            .await??;
        }
        Ok(())
    }

    pub async fn recover_cashu_deposits(&self) -> anyhow::Result<()> {
        // Avoid creating Lightning storage on installations that never use it.
        if !self.path.try_exists()? {
            return Ok(());
        }
        let db = self.database().await?;
        let mints = tokio::task::spawn_blocking(
            move || -> anyhow::Result<Vec<(MintUrl, std::collections::HashSet<String>)>> {
                let tx = db.begin_read()?;
                let mut associated =
                    std::collections::HashMap::<MintUrl, std::collections::HashSet<String>>::new();
                for row in tx.open_table(QUOTES)?.iter()? {
                    let deposit: Deposit = serde_json::from_str(row?.1.value())?;
                    associated
                        .entry(deposit.mint)
                        .or_default()
                        .insert(deposit.quote);
                }
                tx.open_table(MINTS)?
                    .iter()?
                    .map(|row| {
                        let mint = super::super::parse_mint(row?.0.value())?;
                        let quotes = associated.remove(&mint).unwrap_or_default();
                        Ok((mint, quotes))
                    })
                    .collect()
            },
        )
        .await??;
        for (mint, associated) in mints {
            if self
                .payments
                .recover_mint_deposits(&mint, &associated)
                .await
                .is_err()
            {
                tracing::warn!("Cashu L402 deposit recovery deferred");
            }
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.prune_cashu_deposits(),
        )
        .await??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cdk::{
        Amount,
        nuts::{CurrencyUnit, PaymentMethod},
        wallet::types::{Transaction, TransactionDirection, TransactionStatus},
    };

    #[tokio::test]
    async fn confirmed_expired_unpaid_quote_releases_full_challenge_capacity() -> anyhow::Result<()>
    {
        use crate::payments::minting::QuoteStatusFixture;
        let root = tempfile::tempdir()?;
        let payments = Arc::new(crate::payments::Payments::new(root.path()));
        let lightning = Lightning::new(root.path(), payments.clone());
        let p = super::super::tests::policy();
        let invoice = super::super::tests::signed(&p, &digest(b"request"), now()? - 7200);
        let fixture = QuoteStatusFixture::new(&invoice, "unpaid").await?;
        let stored = crate::payments::MintWallet::open(&payments.root, &fixture.mint).await?;
        let store = stored.wallet.localstore.clone();
        store
            .add_mint_quote(cdk::wallet::MintQuote::new(
                "unpaid".into(),
                fixture.mint.clone(),
                PaymentMethod::BOLT11,
                Some(Amount::from(25)),
                CurrencyUnit::Sat,
                invoice.clone(),
                1,
                None,
            ))
            .await?;
        *payments.entry(&fixture.mint).await.lock().await = Some(stored);
        let db = lightning.database().await?;
        let tx = db.begin_write()?;
        {
            let mut roots = tx.open_table(super::super::l402::ROOTS)?;
            for n in 0..MAX_CHALLENGE_RECORDS - 1 {
                roots.insert(format!("root-{n}").as_str(), (&[1u8; 32][..], u64::MAX))?;
            }
            let deposit = serde_json::to_string(&Deposit {
                mint: fixture.mint.clone(),
                quote: "unpaid".into(),
                amount: 25,
                invoice,
                retired: false,
            })?;
            tx.open_table(QUOTES)?
                .insert("expired-unpaid", deposit.as_str())?;
        }
        tx.commit()?;
        assert!(lightning.check_challenge_capacity(1).await.is_err());
        fixture.response.lock().0 = 503;
        lightning.prune_cashu_deposits().await?;
        assert!(lightning.check_challenge_capacity(1).await.is_err());
        assert!(store.get_mint_quote("unpaid").await?.is_some());
        fixture.response.lock().0 = 200;
        lightning.prune_cashu_deposits().await?;
        assert!(lightning.check_challenge_capacity(1).await.is_ok());
        assert!(store.get_mint_quote("unpaid").await?.is_none());
        assert!(
            db.begin_read()?
                .open_table(QUOTES)?
                .get("expired-unpaid")?
                .is_none()
        );
        lightning.prune_cashu_deposits().await?;
        Ok(())
    }

    #[tokio::test]
    async fn cashu_cleanup_retains_uncertain_quotes_and_resumes_retirement() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let payments = Arc::new(crate::payments::Payments::new(root.path()));
        let lightning = Lightning::new(root.path(), payments.clone());
        let mint = crate::payments::parse_mint("https://mint.example.com")?;
        let p = super::super::tests::policy();
        let hash = digest(b"request");
        let expired = super::super::tests::signed(&p, &hash, now()? - 7200);
        let live = super::super::tests::signed(&p, &hash, now()?);
        let stored = crate::payments::MintWallet::open(&payments.root, &mint).await?;
        stored
            .wallet
            .localstore
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
                quote_id: Some("completed".into()),
                payment_request: Some(expired.clone()),
                payment_proof: None,
                payment_method: Some(PaymentMethod::BOLT11),
                saga_id: None,
                status: TransactionStatus::Completed,
            })
            .await?;
        *payments.entry(&mint).await.lock().await = Some(stored);
        let db = lightning.database().await?;
        let tx = db.begin_write()?;
        for (key, quote, invoice) in [
            ("expired-collected", "completed", expired.clone()),
            ("expired-unpaid", "unpaid", expired),
            ("live", "completed", live),
            (
                "retired",
                "missing-after-crash",
                super::super::tests::signed(&p, &hash, now()? - 7200),
            ),
        ] {
            let value = serde_json::to_string(&Deposit {
                mint: mint.clone(),
                quote: quote.into(),
                amount: 25,
                invoice,
                retired: key == "retired",
            })?;
            tx.open_table(QUOTES)?.insert(key, value.as_str())?;
        }
        tx.commit()?;
        lightning.prune_cashu_deposits().await?;
        lightning.prune_cashu_deposits().await?;
        let tx = db.begin_read()?;
        let table = tx.open_table(QUOTES)?;
        assert!(table.get("expired-collected")?.is_none());
        assert!(table.get("expired-unpaid")?.is_some());
        assert!(table.get("live")?.is_some());
        assert!(table.get("retired")?.is_none());
        Ok(())
    }
}
