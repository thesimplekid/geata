mod ledger;
pub mod payout;

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, ensure};
use cdk::{
    Amount, Wallet,
    mint_url::MintUrl,
    nuts::{CurrencyUnit, PaymentRequest, Token},
    wallet::{
        ReceiveOptions,
        types::{TransactionDirection, TransactionStatus},
    },
};
use cdk_sqlite::WalletSqliteDatabase;
use tokio::sync::Mutex;

use ledger::{Ledger, Reservation};

pub const MAX_TOKEN_BYTES: usize = 16 * 1024;
const PAYMENT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentPolicy {
    pub price: u32,
    pub mint: MintUrl,
}

impl PaymentPolicy {
    pub fn parse(price: &str, unit: &str, mint: &str) -> anyhow::Result<Self> {
        let price = price
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .context("payment price must be a positive integer")?;
        ensure!(unit == "sat", "payments currently support sat only");
        Ok(Self {
            price,
            mint: parse_mint(mint)?,
        })
    }

    pub fn challenge(&self) -> String {
        PaymentRequest::builder()
            .amount(u64::from(self.price))
            .unit(CurrencyUnit::Sat)
            .add_mint(self.mint.clone())
            .description("One Geata request attempt")
            .build()
            .to_string()
    }
}

pub fn parse_mint(value: &str) -> anyhow::Result<MintUrl> {
    let url = url::Url::parse(value).context("invalid Cashu mint URL")?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "Cashu mint must use HTTPS (HTTP is allowed for loopback tests)"
    );
    ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Cashu mint URL must not contain credentials, query, or fragment"
    );
    Ok(MintUrl::from_str(url.as_str())?)
}

#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    #[error("Invalid, insufficient, or already used Cashu payment.")]
    Invalid,
    #[error("Cashu payments are temporarily unavailable; retry the same token.")]
    Unavailable,
}

impl From<anyhow::Error> for PaymentError {
    fn from(_: anyhow::Error) -> Self {
        // Mint responses and wallet errors can contain bearer tokens: never log them.
        Self::Unavailable
    }
}

pub struct Payments {
    root: PathBuf,
    wallets: Mutex<HashMap<MintUrl, Arc<Mutex<Option<MintWallet>>>>>,
}

struct MintWallet {
    wallet: Wallet,
    ledger: Ledger,
    needs_recovery: bool,
}

impl Payments {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("payments"),
            wallets: Mutex::new(HashMap::new()),
        }
    }

    async fn entry(&self, mint: &MintUrl) -> Arc<Mutex<Option<MintWallet>>> {
        self.wallets
            .lock()
            .await
            .entry(mint.clone())
            .or_default()
            .clone()
    }

    pub async fn collect(
        &self,
        site: &str,
        policy: &PaymentPolicy,
        encoded: &str,
    ) -> Result<(), PaymentError> {
        tokio::time::timeout(PAYMENT_TIMEOUT, self.collect_inner(site, policy, encoded))
            .await
            .unwrap_or(Err(PaymentError::Unavailable))
    }

    async fn collect_inner(
        &self,
        site: &str,
        policy: &PaymentPolicy,
        encoded: &str,
    ) -> Result<(), PaymentError> {
        let token = validate_token(policy, encoded)?;
        let id = token_id(&token);
        let entry = self.entry(&policy.mint).await;
        // One mint operation at a time, with no unbounded queue or recovery races.
        let mut guard = entry.try_lock().map_err(|_| PaymentError::Unavailable)?;
        if guard.is_none() {
            *guard = Some(MintWallet::open(&self.root, &policy.mint).await?);
        }
        let stored = guard.as_mut().ok_or(PaymentError::Unavailable)?;
        stored.recover().await?;
        stored
            .wallet
            .verify_token_dleq(&token)
            .await
            .map_err(classify_error)?;
        let keysets = stored
            .wallet
            .localstore
            .get_mint_keysets(policy.mint.clone())
            .await
            .map_err(|_| PaymentError::Unavailable)?
            .unwrap_or_default();
        let proofs = token.proofs(&keysets).map_err(|_| PaymentError::Invalid)?;
        let fees = stored
            .wallet
            .get_proofs_fee(&proofs)
            .await
            .map_err(classify_error)?
            .total;
        let net = token
            .value()
            .map_err(|_| PaymentError::Invalid)?
            .checked_sub(fees)
            .ok_or(PaymentError::Invalid)?;
        if net < Amount::from(u64::from(policy.price)) {
            return Err(PaymentError::Invalid);
        }
        let memo = format!("geata-payment:{id}");
        match stored.ledger.reserve(&id, site, policy.price).await? {
            Reservation::Claimed | Reservation::Mismatch => return Err(PaymentError::Invalid),
            Reservation::Pending => {
                // A previous request may have completed redemption just before a crash
                // or disconnection. CDK recovery establishes ownership of those funds.
                let transactions = stored
                    .wallet
                    .list_transactions(Some(TransactionDirection::Incoming))
                    .await
                    .map_err(classify_error)?;
                if transactions.iter().any(|tx| {
                    tx.memo.as_deref() == Some(&memo)
                        && tx.status == TransactionStatus::Completed
                        && tx.amount >= Amount::from(u64::from(policy.price))
                }) {
                    return stored.claim(&id).await;
                }
            }
            Reservation::New => {}
        }
        stored.needs_recovery = true;
        // This flag remains set if the future is cancelled or redemption fails.
        let received = stored
            .wallet
            .receive_proofs(
                proofs,
                ReceiveOptions::default(),
                Some(memo),
                Some(encoded.to_owned()),
            )
            .await
            .map_err(classify_error)?;
        if received < Amount::from(u64::from(policy.price)) {
            return Err(PaymentError::Unavailable);
        }
        stored.needs_recovery = false;
        stored.claim(&id).await
    }

    pub async fn balance(&self, mint: &MintUrl) -> anyhow::Result<Amount> {
        let mut wallet = MintWallet::open(&self.root, mint).await?;
        wallet.recover().await?;
        Ok(wallet.wallet.total_balance().await?)
    }

    pub async fn export(&self, mint: &MintUrl, amount: u64) -> anyhow::Result<String> {
        let mut wallet = MintWallet::open(&self.root, mint).await?;
        wallet.recover().await?;
        let send = wallet
            .wallet
            .prepare_send(Amount::from(amount), Default::default())
            .await?;
        Ok(send.confirm(None).await?.to_string())
    }
}

impl MintWallet {
    async fn open(root: &Path, mint: &MintUrl) -> anyhow::Result<Self> {
        let directory = root.join(digest(mint.to_string().as_bytes()));
        let setup = directory.clone();
        let (seed, ledger) = tokio::task::spawn_blocking(move || prepare_storage(&setup)).await??;
        let store = WalletSqliteDatabase::new(directory.join("wallet.sqlite")).await?;
        Ok(Self {
            wallet: Wallet::new(
                &mint.to_string(),
                CurrencyUnit::Sat,
                Arc::new(store),
                seed,
                None,
            )?,
            ledger,
            needs_recovery: true,
        })
    }

    async fn recover(&mut self) -> Result<(), PaymentError> {
        if self.needs_recovery {
            let report = self
                .wallet
                .recover_incomplete_sagas()
                .await
                .map_err(classify_error)?;
            if report.failed != 0 || report.skipped != 0 {
                return Err(PaymentError::Unavailable);
            }
            self.needs_recovery = false;
        }
        Ok(())
    }

    async fn claim(&self, id: &str) -> Result<(), PaymentError> {
        if self.ledger.claim(id).await? {
            Ok(())
        } else {
            Err(PaymentError::Invalid)
        }
    }
}

fn classify_error(error: cdk::Error) -> PaymentError {
    match error {
        cdk::Error::TokenAlreadySpent
        | cdk::Error::CouldNotVerifyDleq
        | cdk::Error::DleqProofNotProvided
        | cdk::Error::AmountKey => PaymentError::Invalid,
        _ => PaymentError::Unavailable,
    }
}

fn validate_token(policy: &PaymentPolicy, encoded: &str) -> Result<Token, PaymentError> {
    if encoded.len() > MAX_TOKEN_BYTES || !encoded.starts_with("cashuB") {
        return Err(PaymentError::Invalid);
    }
    let token = Token::from_str(encoded).map_err(|_| PaymentError::Invalid)?;
    if token.unit() != Some(CurrencyUnit::Sat)
        || token.mint_url().map_err(|_| PaymentError::Invalid)? != policy.mint
        || token.value().map_err(|_| PaymentError::Invalid)? < Amount::from(u64::from(policy.price))
        || !token
            .spending_conditions()
            .map_err(|_| PaymentError::Invalid)?
            .is_empty()
    {
        return Err(PaymentError::Invalid);
    }
    Ok(token)
}

fn token_id(token: &Token) -> String {
    // Ignore memo, encoding and proof order, so alternate encodings share admission.
    let mut secrets: Vec<_> = token
        .token_secrets()
        .iter()
        .map(|s| s.to_string())
        .collect();
    secrets.sort();
    let mut input = Vec::new();
    for secret in secrets {
        input.extend_from_slice(&(secret.len() as u64).to_be_bytes());
        input.extend_from_slice(secret.as_bytes());
    }
    digest(&input)
}

fn digest(input: &[u8]) -> String {
    openssl::sha::sha256(input)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn private_directory(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        private_directory(parent)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "payment directory must be a real private directory (0700)"
    );
    Ok(())
}

fn private_file(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
            "payment file must be a real private file (0600)"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn prepare_storage(directory: &Path) -> anyhow::Result<([u8; 64], Ledger)> {
    private_directory(
        directory
            .parent()
            .context("payment directory has no parent")?,
    )?;
    private_directory(directory)?;
    let seed_path = directory.join("seed");
    if !seed_path.try_exists()? {
        ensure!(
            !directory.join("wallet.sqlite").try_exists()?
                && !directory.join("ledger.sqlite").try_exists()?
                && !directory.join("ledger.redb").try_exists()?,
            "payment seed is missing; restore it from backup"
        );
        let mut seed = [0; 64];
        openssl::rand::rand_bytes(&mut seed)?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        temporary.write_all(&seed)?;
        temporary.as_file().sync_all()?;
        temporary.persist_noclobber(&seed_path)?;
        fs::File::open(directory)?.sync_all()?;
    }
    private_file(&seed_path)?;
    let seed: [u8; 64] = fs::read(seed_path)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid payment seed"))?;
    private_file(&directory.join("ledger.redb"))?;
    for name in ["wallet.sqlite", "ledger.sqlite"] {
        let path = directory.join(name);
        if name == "ledger.sqlite" {
            match fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
                Ok(_) => {}
            }
        }
        private_file(&path)?;
        for suffix in ["-wal", "-shm"] {
            let path = directory.join(format!("{name}{suffix}"));
            if path.try_exists()? {
                private_file(&path)?;
            }
        }
    }
    Ok((seed, Ledger::open(&directory.join("ledger.redb"))?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn policy_is_strict_and_produces_a_standard_cashu_challenge() -> anyhow::Result<()> {
        let policy = PaymentPolicy::parse("2", "sat", "https://mint.example.com")?;
        let request = PaymentRequest::from_str(&policy.challenge())?;
        assert_eq!(request.amount, Some(Amount::from(2)));
        assert_eq!(request.unit, Some(CurrencyUnit::Sat));
        assert_eq!(request.mints, vec![policy.mint]);
        assert!(request.transports.is_empty());
        for (price, unit, mint) in [
            ("0", "sat", "https://mint.example.com"),
            ("1", "usd", "https://mint.example.com"),
            ("1", "sat", "http://mint.example.com"),
            ("1", "sat", "https://user:pass@mint.example.com"),
            ("1", "sat", "https://mint.example.com?token=secret"),
            ("1", "sat", "https://mint.example.com#fragment"),
        ] {
            assert!(PaymentPolicy::parse(price, unit, mint).is_err());
        }
        assert!(parse_mint("http://127.0.0.1:3338").is_ok());
        Ok(())
    }

    #[test]
    fn payment_storage_is_private_and_missing_seeds_are_never_replaced() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let directory = root.path().join("payments").join("mint");
        let (seed, ledger) = prepare_storage(&directory)?;
        drop(ledger);
        assert_eq!(prepare_storage(&directory)?.0, seed);
        assert!(!directory.join("ledger.sqlite").exists());
        for name in ["seed", "wallet.sqlite", "ledger.redb"] {
            assert_eq!(
                fs::metadata(directory.join(name))?.permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_file(directory.join("wallet.sqlite"))?;
        fs::remove_file(directory.join("seed"))?;
        assert!(prepare_storage(&directory).is_err());
        assert!(!directory.join("seed").exists());
        Ok(())
    }

    #[test]
    fn payment_storage_rejects_symlinked_files_and_directories() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let target = root.path().join("target");
        fs::create_dir(&target)?;
        let link = root.path().join("payments");
        symlink(&target, &link)?;
        assert!(prepare_storage(&link.join("mint")).is_err());
        assert!(!target.join("mint").exists());
        let file = root.path().join("file");
        let missing = root.path().join("missing");
        symlink(&missing, &file)?;
        assert!(private_file(&file).is_err());
        assert!(!missing.exists());
        Ok(())
    }
}
