//! Expiry-based cleanup never evicts valid credentials or live replay barriers.
use super::*;

pub(super) fn prune(tx: &redb::WriteTransaction, time: u64) -> anyhow::Result<()> {
    tx.open_table(l402::ROOTS)?
        .retain(|_, (_, expiry)| expiry > time)?;
    tx.open_table(SPENT)?.retain(|_, expiry| expiry > time)?;
    Ok(())
}

pub(super) fn check_capacity(tx: &redb::WriteTransaction, needed: u64) -> anyhow::Result<()> {
    use redb::ReadableTableMetadata;
    let count = tx
        .open_table(l402::ROOTS)?
        .len()?
        .saturating_add(tx.open_table(cashu::QUOTES)?.len()?);
    ensure!(
        count.saturating_add(needed) <= MAX_CHALLENGE_RECORDS,
        "outstanding challenge capacity exhausted"
    );
    Ok(())
}

impl Lightning {
    pub(super) async fn check_challenge_capacity(&self, needed: u64) -> anyhow::Result<()> {
        let db = self.database().await?;
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let tx = db.begin_write()?;
            check_capacity(&tx, needed)
        })
        .await?
    }

    pub async fn prune_expired(&self) -> anyhow::Result<()> {
        // Do not create payment storage for installations without Lightning.
        if !self.path.try_exists()? {
            return Ok(());
        }
        let db = self.database().await?;
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut tx = db.begin_write()?;
            tx.set_durability(Durability::Immediate)?;
            prune(&tx, now()?)?;
            tx.commit()?;
            Ok(())
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::{ReadableDatabase, ReadableTableMetadata};

    fn service(path: &Path) -> Lightning {
        Lightning::new(path, Arc::new(crate::payments::Payments::new(path)))
    }

    #[tokio::test]
    async fn startup_and_periodic_pruning_preserve_live_roots_and_replay_barriers()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let lightning = service(root.path());
        let db = lightning.database().await?;
        let time = now()?;
        let tx = db.begin_write()?;
        {
            let mut roots = tx.open_table(l402::ROOTS)?;
            roots.insert("expired", (&[1u8; 32][..], time))?;
            roots.insert("live", (&[2u8; 32][..], time + 86400))?;
            let mut spent = tx.open_table(SPENT)?;
            spent.insert("expired", time)?;
            spent.insert("live", time + 86400)?;
            tx.open_table(cashu::QUOTES)?
                .insert("uncertain", "retain for recovery")?;
        }
        tx.commit()?;
        drop(db);
        drop(lightning);
        let lightning = service(root.path());
        let db = lightning.database().await?;
        {
            let tx = db.begin_read()?;
            assert!(tx.open_table(l402::ROOTS)?.get("expired")?.is_none());
            assert!(tx.open_table(l402::ROOTS)?.get("live")?.is_some());
            assert!(tx.open_table(SPENT)?.get("expired")?.is_none());
            assert!(tx.open_table(SPENT)?.get("live")?.is_some());
            assert!(tx.open_table(cashu::QUOTES)?.get("uncertain")?.is_some());
        }
        assert!(matches!(
            lightning.claim("live".into(), time + 86400).await,
            Err(Error::Invalid)
        ));
        let tx = db.begin_write()?;
        tx.open_table(l402::ROOTS)?
            .insert("later-expired", (&[3u8; 32][..], time))?;
        tx.open_table(SPENT)?.insert("later-expired", time)?;
        tx.commit()?;
        lightning.prune_expired().await?;
        let tx = db.begin_read()?;
        assert!(tx.open_table(l402::ROOTS)?.get("later-expired")?.is_none());
        assert!(tx.open_table(SPENT)?.get("later-expired")?.is_none());
        assert!(lightning.claim("expired".into(), time).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn challenge_capacity_is_durable_and_checked_before_issuance() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let lightning = service(root.path());
        let db = lightning.database().await?;
        let tx = db.begin_write()?;
        {
            let mut roots = tx.open_table(l402::ROOTS)?;
            for n in 0..MAX_CHALLENGE_RECORDS - 1 {
                roots.insert(format!("root-{n}").as_str(), (&[1u8; 32][..], u64::MAX))?;
            }
            tx.open_table(cashu::QUOTES)?
                .insert("uncertain", "retained")?;
        }
        tx.commit()?;
        let mut p = super::super::tests::policy();
        p.protocols.l402 = true;
        assert!(lightning.check_challenge_capacity(0).await.is_ok());
        let hash = digest(b"request");
        let error = lightning
            .challenge(&p, "https://api.example.com/", &hash, [192, 0, 2, 1].into())
            .await
            .err()
            .expect("capacity error");
        assert!(error.to_string().contains("outstanding challenge capacity"));
        let invoice = super::super::tests::signed(&p, &hash, now()?);
        let parsed = invoice.parse()?;
        assert!(
            lightning
                .mint_l402(&p, &hash, &invoice, &parsed)
                .await
                .is_err()
        );
        // Free one slot; concurrent inserts must not overbook it.
        let tx = db.begin_write()?;
        tx.open_table(l402::ROOTS)?.remove("root-0")?;
        tx.commit()?;
        let (a, b) = tokio::join!(
            lightning.mint_l402(&p, &hash, &invoice, &parsed),
            lightning.mint_l402(&p, &hash, &invoice, &parsed),
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        assert_eq!(
            db.begin_read()?.open_table(l402::ROOTS)?.len()?,
            MAX_CHALLENGE_RECORDS - 1
        );
        drop(db);
        drop(lightning);
        assert!(
            service(root.path())
                .check_challenge_capacity(1)
                .await
                .is_err()
        );
        Ok(())
    }
}
