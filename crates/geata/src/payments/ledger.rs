use std::{fs, path::Path, sync::Arc};

use anyhow::ensure;
use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
};
use rusqlite::{Connection, OpenFlags};

// Version the table names so incompatible record layouts cannot be read silently.
const PAYMENTS: TableDefinition<&str, (&str, u32, bool)> = TableDefinition::new("payments_v1");
const PAYOUT: TableDefinition<&str, &str> = TableDefinition::new("payout_v1");
const METADATA: TableDefinition<&str, u8> = TableDefinition::new("metadata");

#[derive(Clone)]
pub struct Ledger(Arc<Database>);

#[derive(Debug, PartialEq, Eq)]
pub enum Reservation {
    New,
    Pending,
    Claimed,
    Mismatch,
}

impl Ledger {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let database = Database::create(path)?;
        let mut tx = database.begin_write()?;
        tx.set_durability(Durability::Immediate)?;
        {
            let mut metadata = tx.open_table(METADATA)?;
            let version = metadata.get("version")?.map(|value| value.value());
            match version {
                Some(1) => {}
                Some(_) => anyhow::bail!("unsupported payment ledger version"),
                None => {
                    let mut payments = tx.open_table(PAYMENTS)?;
                    ensure!(
                        payments.is_empty()?,
                        "unversioned payment ledger is not empty"
                    );
                    let legacy = path.with_extension("sqlite");
                    if legacy.try_exists()? {
                        // Import once, atomically with the version marker. Never modify
                        // the old ledger or overwrite newer redb admission decisions.
                        let connection =
                            Connection::open_with_flags(legacy, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                        let mut query =
                            connection.prepare("SELECT id, site, price, claimed FROM payments")?;
                        let mut rows = query.query([])?;
                        while let Some(row) = rows.next()? {
                            let id: String = row.get(0)?;
                            let site: String = row.get(1)?;
                            let price: u32 = row.get(2)?;
                            let claimed: i64 = row.get(3)?;
                            ensure!(
                                price > 0 && matches!(claimed, 0 | 1),
                                "invalid legacy payment record"
                            );
                            payments.insert(id.as_str(), (site.as_str(), price, claimed == 1))?;
                        }
                    }
                    metadata.insert("version", 1)?;
                }
            }
        }
        tx.open_table(PAYOUT)?;
        tx.commit()?;
        // Persist the directory entry as well as the database's contents.
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(Self(Arc::new(database)))
    }

    async fn run<T, F>(&self, operation: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Database) -> anyhow::Result<T> + Send + 'static,
    {
        let database = self.0.clone();
        tokio::task::spawn_blocking(move || operation(&database)).await?
    }

    pub async fn payout(&self) -> anyhow::Result<Option<super::payout::PayoutState>> {
        self.run(|database| {
            let tx = database.begin_read()?;
            let table = tx.open_table(PAYOUT)?;
            table
                .get("state")?
                .map(|v| serde_json::from_str(v.value()).map_err(Into::into))
                .transpose()
        })
        .await
    }

    pub async fn save_payout(&self, state: &super::payout::PayoutState) -> anyhow::Result<()> {
        let value = serde_json::to_string(state)?;
        self.run(move |database| {
            let mut tx = database.begin_write()?;
            tx.set_durability(Durability::Immediate)?;
            tx.open_table(PAYOUT)?.insert("state", value.as_str())?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn reserve(&self, id: &str, site: &str, price: u32) -> anyhow::Result<Reservation> {
        let (id, site) = (id.to_owned(), site.to_owned());
        self.run(move |database| {
            let mut tx = database.begin_write()?;
            tx.set_durability(Durability::Immediate)?;
            let result = {
                let mut payments = tx.open_table(PAYMENTS)?;
                let existing = payments.get(id.as_str())?.map(|value| {
                    let (site, price, claimed) = value.value();
                    (site.to_owned(), price, claimed)
                });
                match existing {
                    Some((old_site, old_price, _)) if old_site != site || old_price != price => {
                        Reservation::Mismatch
                    }
                    Some((_, _, true)) => Reservation::Claimed,
                    Some(_) => Reservation::Pending,
                    None => {
                        payments.insert(id.as_str(), (site.as_str(), price, false))?;
                        Reservation::New
                    }
                }
            };
            tx.commit()?;
            Ok(result)
        })
        .await
    }

    /// Persist admission before the handler runs. A crash may lose an attempt,
    /// but must never authorize a second attempt with an already used payment.
    pub async fn claim(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_owned();
        self.run(move |database| {
            let mut tx = database.begin_write()?;
            tx.set_durability(Durability::Immediate)?;
            let claimed = {
                let mut payments = tx.open_table(PAYMENTS)?;
                let existing = payments.get(id.as_str())?.map(|value| {
                    let (site, price, claimed) = value.value();
                    (site.to_owned(), price, claimed)
                });
                if let Some((site, price, false)) = existing {
                    payments.insert(id.as_str(), (site.as_str(), price, true))?;
                    true
                } else {
                    false
                }
            };
            tx.commit()?;
            Ok(claimed)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn claims_are_exclusive_persistent_and_bound_to_site_and_price() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("ledger.redb");
        let ledger = Ledger::open(&path)?;
        assert_eq!(ledger.reserve("token", "site", 2).await?, Reservation::New);
        assert_eq!(
            ledger.reserve("token", "other", 2).await?,
            Reservation::Mismatch
        );
        assert_eq!(
            ledger.reserve("token", "site", 3).await?,
            Reservation::Mismatch
        );
        let (first, second) = tokio::join!(ledger.claim("token"), ledger.claim("token"));
        assert_ne!(first?, second?);
        drop(ledger);
        let ledger = Ledger::open(&path)?;
        assert_eq!(
            ledger.reserve("token", "site", 2).await?,
            Reservation::Claimed
        );
        assert_eq!(
            ledger.reserve("pending", "site", 2).await?,
            Reservation::New
        );
        drop(ledger);
        let ledger = Ledger::open(&path)?;
        assert_eq!(
            ledger.reserve("pending", "site", 2).await?,
            Reservation::Pending
        );
        Ok(())
    }

    #[tokio::test]
    async fn legacy_import_preserves_records_and_does_not_repeat() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let legacy = root.path().join("ledger.sqlite");
        let connection = Connection::open(&legacy)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE payments (id TEXT PRIMARY KEY, site TEXT, price INTEGER, claimed INTEGER);
             INSERT INTO payments VALUES ('used', 'site', 2, 1), ('pending', 'site', 3, 0);",
        )?;
        // Keep SQLite open so the import must also see committed WAL records.
        let path = root.path().join("ledger.redb");
        let ledger = Ledger::open(&path)?;
        assert_eq!(
            ledger.reserve("used", "site", 2).await?,
            Reservation::Claimed
        );
        assert_eq!(
            ledger.reserve("used", "other", 2).await?,
            Reservation::Mismatch
        );
        assert_eq!(
            ledger.reserve("pending", "site", 2).await?,
            Reservation::Mismatch
        );
        assert_eq!(
            ledger.reserve("pending", "site", 3).await?,
            Reservation::Pending
        );
        assert!(ledger.claim("pending").await?);
        assert!(!ledger.claim("missing").await?);
        drop(ledger);
        let old_claimed: bool = connection.query_row(
            "SELECT claimed FROM payments WHERE id = 'pending'",
            [],
            |row| row.get(0),
        )?;
        assert!(
            !old_claimed,
            "migration must leave the SQLite records unchanged"
        );
        drop(connection);
        let ledger = Ledger::open(&path)?;
        assert_eq!(
            ledger.reserve("pending", "site", 3).await?,
            Reservation::Claimed
        );
        drop(ledger);
        // A completed import no longer depends on the old database.
        fs::write(&legacy, b"unreadable old backup")?;
        let ledger = Ledger::open(&path)?;
        assert_eq!(
            ledger.reserve("used", "site", 2).await?,
            Reservation::Claimed
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_import_rolls_back_and_can_be_retried() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let connection = Connection::open(root.path().join("ledger.sqlite"))?;
        connection.execute_batch(
            "CREATE TABLE payments (id TEXT PRIMARY KEY, site TEXT, price INTEGER, claimed INTEGER);
             INSERT INTO payments VALUES ('used', 'site', 2, 1), ('invalid', 'site', 2, 7);",
        )?;
        let path = root.path().join("ledger.redb");
        assert!(Ledger::open(&path).is_err());
        connection.execute("DELETE FROM payments WHERE id = 'invalid'", [])?;
        let ledger = Ledger::open(&path)?;
        assert_eq!(
            ledger.reserve("used", "site", 2).await?,
            Reservation::Claimed
        );
        assert_eq!(
            ledger.reserve("invalid", "site", 2).await?,
            Reservation::New
        );
        Ok(())
    }
}
