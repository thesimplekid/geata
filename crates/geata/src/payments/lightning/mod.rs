//! x402 v2 exact/lnbtc, HTTP request binding and local upfront settlement.
mod cashu;
mod l402;
mod receiver;

use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use bitcoin::{hashes::Hash, secp256k1::PublicKey};
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescriptionRef, Currency};
use redb::{Database, Durability, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

const MAINNET: &str = "lnbtc:000000000019d6689c085ae165831e93";
const TESTNET: &str = "lnbtc:000000000933ea01ad0ee984209779ba";
const SPENT: TableDefinition<&str, u64> = TableDefinition::new("lightning_spent_v1");
const SKEW: u64 = 60;
pub const MAX_PAYMENT_HEADER: usize = 24 * 1024;
pub const MAX_BODY: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct LdkReceiverConfig {
    pub endpoint: String,
    pub api_key_file: PathBuf,
    pub tls_cert_file: PathBuf,
    pub pay_to: String,
    pub network: String,
    pub expiry_seconds: u32,
}

#[derive(Clone, Debug)]
pub enum ReceiverConfig {
    Ldk(LdkReceiverConfig),
    Cashu {
        mint: cdk::mint_url::MintUrl,
        network: String,
    },
}
impl ReceiverConfig {
    pub fn network(&self) -> &str {
        match self {
            Self::Ldk(node) => &node.network,
            Self::Cashu { network, .. } => network,
        }
    }
    pub fn ldk(&self) -> Option<&LdkReceiverConfig> {
        match self {
            Self::Ldk(node) => Some(node),
            _ => None,
        }
    }
    pub fn mint(&self) -> Option<&cdk::mint_url::MintUrl> {
        match self {
            Self::Cashu { mint, .. } => Some(mint),
            _ => None,
        }
    }
    fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Ldk(node) => node.validate(),
            Self::Cashu { mint, network } => {
                super::parse_mint(&mint.to_string())?;
                ensure!(
                    matches!(network.as_str(), MAINNET | TESTNET),
                    "unsupported mint network"
                );
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub amount_msat: u64,
    pub receiver: ReceiverConfig,
    pub origin: String,
    pub headers: Vec<String>,
    pub protocols: Protocols,
}
#[derive(Clone, Debug)]
pub struct Protocols {
    pub x402: bool,
    pub l402: bool,
}
impl Default for Protocols {
    fn default() -> Self {
        Self {
            x402: true,
            l402: false,
        }
    }
}
impl Protocols {
    pub fn parse(names: &[&str]) -> anyhow::Result<Self> {
        ensure!(
            !names.is_empty(),
            "lightning_protocols requires x402, l402, or both"
        );
        let mut protocols = Self {
            x402: false,
            l402: false,
        };
        for name in names {
            let enabled = match *name {
                "x402" => &mut protocols.x402,
                "l402" => &mut protocols.l402,
                _ => anyhow::bail!("unknown Lightning protocol: {name}"),
            };
            ensure!(!*enabled, "duplicate Lightning protocol: {name}");
            *enabled = true;
        }
        Ok(protocols)
    }
}
pub struct Challenge {
    pub x402: Option<String>,
    pub l402: Option<String>,
}

impl Policy {
    pub fn new(
        price: &str,
        unit: &str,
        receiver: ReceiverConfig,
        origin: String,
        headers: Vec<String>,
        protocols: Protocols,
    ) -> anyhow::Result<Self> {
        ensure!(
            !price.is_empty() && price.bytes().all(|b| b.is_ascii_digit()),
            "Lightning price must be a positive integer"
        );
        let price: u64 = price.parse()?;
        ensure!(
            price > 0 && unit == "sat",
            "Lightning price must use positive whole sats"
        );
        receiver.validate()?;
        let policy = Self {
            amount_msat: price
                .checked_mul(1000)
                .context("Lightning price overflow")?,
            receiver,
            origin,
            headers,
            protocols,
        };
        policy.validate_binding()?;
        Ok(policy)
    }
    pub fn check_site(&self, domain: &str, https: bool) -> anyhow::Result<()> {
        let origin = url::Url::parse(&self.origin)?;
        ensure!(
            origin.host_str() == Some(domain) && (origin.scheme() == "https") == https,
            "Lightning origin must match its Geata site"
        );
        Ok(())
    }
    pub fn requirements(&self, hash: String, invoice: String) -> Requirements {
        Requirements {
            scheme: "exact".into(),
            network: self.receiver.network().to_owned(),
            amount: self.amount_msat.to_string(),
            asset: "BTC".into(),
            pay_to: self
                .receiver
                .ldk()
                .map(|node| node.pay_to.clone())
                .unwrap_or_default(),
            max_timeout_seconds: self
                .receiver
                .ldk()
                .map(|node| node.expiry_seconds)
                .unwrap_or_default(),
            extra: Extra {
                asset_transfer_method: Some("bolt11".into()),
                payment_flow: "upfront".into(),
                request_hash: hash,
                request_binding_profile: "http:1".into(),
                request_binding_params: BindingParams {
                    headers: self.headers.clone(),
                },
                invoice,
            },
        }
    }
}
impl ReceiverConfig {
    pub fn parse(args: &[&str]) -> anyhow::Result<Self> {
        ensure!(
            args.len().is_multiple_of(2),
            "Lightning receiver settings need a name and value"
        );
        let mut fields = std::collections::BTreeMap::new();
        for pair in args.chunks_exact(2) {
            ensure!(
                matches!(
                    pair[0],
                    "mint"
                        | "endpoint"
                        | "api_key_file"
                        | "tls_cert_file"
                        | "pay_to"
                        | "network"
                        | "expiry_seconds"
                ),
                "unknown Lightning receiver setting: {}",
                pair[0]
            );
            ensure!(
                fields.insert(pair[0], pair[1]).is_none(),
                "duplicate Lightning receiver setting: {}",
                pair[0]
            );
        }
        let required = |key| {
            fields
                .get(key)
                .copied()
                .with_context(|| format!("missing Lightning receiver setting: {key}"))
        };
        let network = match required("network")? {
            "mainnet" => MAINNET,
            "testnet" => TESTNET,
            _ => anyhow::bail!("Lightning network must be mainnet or testnet"),
        };
        if let Some(mint) = fields.get("mint") {
            ensure!(
                fields.keys().all(|key| matches!(*key, "mint" | "network")),
                "Cashu receivers accept only mint and network; do not mix LDK settings"
            );
            return Ok(Self::Cashu {
                mint: super::parse_mint(mint)?,
                network: network.into(),
            });
        }
        let receiver = LdkReceiverConfig {
            endpoint: required("endpoint")?.into(),
            api_key_file: required("api_key_file")?.into(),
            tls_cert_file: required("tls_cert_file")?.into(),
            pay_to: required("pay_to")?.into(),
            network: network.into(),
            expiry_seconds: fields
                .get("expiry_seconds")
                .map_or(Ok(300), |value| value.parse::<u32>())?,
        };
        receiver.validate()?;
        Ok(Self::Ldk(receiver))
    }
}
impl LdkReceiverConfig {
    fn validate(&self) -> anyhow::Result<()> {
        let endpoint = url::Url::parse(&self.endpoint)?;
        ensure!(
            endpoint.scheme() == "https"
                && endpoint.host_str().is_some()
                && endpoint.username().is_empty()
                && endpoint.password().is_none()
                && endpoint.query().is_none()
                && endpoint.fragment().is_none()
                && endpoint.path() == "/",
            "ldk-server endpoint must be an HTTPS origin"
        );
        ensure!(
            self.api_key_file.is_absolute() && self.tls_cert_file.is_absolute(),
            "receiver credential paths must be absolute"
        );
        ensure!(
            matches!(self.network.as_str(), MAINNET | TESTNET),
            "x402 Lightning supports mainnet or testnet only"
        );
        ensure!(
            self.pay_to.len() == 66
                && lowercase_hex(&self.pay_to)
                && self.pay_to.parse::<PublicKey>().is_ok(),
            "pay_to must be a compressed lowercase node public key"
        );
        ensure!(
            self.expiry_seconds > 0 && self.expiry_seconds <= 86400,
            "invoice expiry must be 1..86400 seconds"
        );
        Ok(())
    }
}
impl Policy {
    fn validate_binding(&self) -> anyhow::Result<()> {
        ensure!(
            self.receiver.mint().is_none() || (self.protocols.l402 && !self.protocols.x402),
            "Cashu receivers require lightning_protocols l402; Lightning x402 requires LDK Server"
        );
        ensure!(
            !self.protocols.l402 || !self.headers.iter().any(|h| h == "authorization"),
            "L402 reserves Authorization for payment; remove authorization from lightning_headers and use a bound cookie or custom header for backend authentication"
        );
        let origin = url::Url::parse(&self.origin)?;
        ensure!(
            matches!(origin.scheme(), "http" | "https")
                && origin.host_str().is_some()
                && origin.username().is_empty()
                && origin.password().is_none()
                && origin.query().is_none()
                && origin.fragment().is_none()
                && origin.path() == "/"
                && self.origin == origin.origin().ascii_serialization(),
            "origin must be a canonical HTTP(S) origin without trailing slash"
        );
        ensure!(
            self.headers.windows(2).all(|p| p[0] < p[1]),
            "bound headers must be sorted and unique"
        );
        ensure!(
            self.headers
                .iter()
                .all(|name| name == &name.to_ascii_lowercase()
                    && http::header::HeaderName::from_bytes(name.as_bytes()).is_ok()
                    && !matches!(
                        name.as_str(),
                        "payment-signature"
                            | "x-cashu"
                            | "connection"
                            | "transfer-encoding"
                            | "trailer"
                    )),
            "invalid bound header name"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Requirements {
    scheme: String,
    network: String,
    amount: String,
    asset: String,
    pay_to: String,
    max_timeout_seconds: u32,
    extra: Extra,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Extra {
    #[serde(default)]
    asset_transfer_method: Option<String>,
    payment_flow: String,
    request_hash: String,
    request_binding_profile: String,
    request_binding_params: BindingParams,
    invoice: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct BindingParams {
    headers: Vec<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payment {
    x402_version: u32,
    accepted: Requirements,
    payload: Proof,
}
#[derive(Deserialize)]
struct Proof {
    preimage: String,
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn lowercase_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn decode_hash(value: &str) -> anyhow::Result<[u8; 32]> {
    ensure!(
        value.len() == 64 && lowercase_hex(value),
        "invalid 32-byte lowercase hash"
    );
    let mut bytes = [0; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16)?;
    }
    Ok(bytes)
}
fn digest(bytes: &[u8]) -> String {
    hex(&openssl::sha::sha256(bytes))
}
fn now() -> anyhow::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

/// The HTTP binding object has ASCII property names and only strings/arrays/objects.
/// serde_json's sorted maps and string encoding therefore produce its JCS encoding.
pub fn request_hash(
    policy: &Policy,
    method: &str,
    url: &str,
    headers: &http::HeaderMap,
    body: &[u8],
) -> anyhow::Result<String> {
    ensure!(
        url.is_ascii() && url.starts_with(&format!("{}/", policy.origin)) && !url.contains('#'),
        "request origin mismatch"
    );
    let mut bound = Vec::new();
    for name in &policy.headers {
        let mut values = Vec::new();
        for value in headers.get_all(name).iter() {
            let value = value.to_str()?;
            ensure!(
                value
                    .bytes()
                    .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b)),
                "unsupported bound header value"
            );
            values.push(value.trim_matches([' ', '\t']));
        }
        let value_hash = if values.is_empty() {
            digest(&[0])
        } else {
            let mut bytes = vec![1];
            bytes.extend_from_slice(values.join(", ").as_bytes());
            digest(&bytes)
        };
        bound.push(json!({"name": name, "valueHash": value_hash}));
    }
    Ok(digest(&serde_json::to_vec(&json!({
        "domain": "x402:exact:lnbtc:bolt11:http:1", "method": method,
        "url": url, "bodyHash": digest(body), "headers": bound
    }))?))
}

fn validate_invoice(
    policy: &Policy,
    hash: &str,
    encoded: &str,
    time: u64,
    grace: u64,
) -> anyhow::Result<Bolt11Invoice> {
    let invoice: Bolt11Invoice = encoded.parse().context("invalid BOLT11 invoice")?;
    invoice.check_signature()?;
    if let Some(node) = policy.receiver.ldk() {
        ensure!(
            invoice.recover_payee_pub_key().to_string() == node.pay_to,
            "invoice receiver mismatch"
        );
    }
    ensure!(
        invoice.currency()
            == if policy.receiver.network() == MAINNET {
                Currency::Bitcoin
            } else {
                Currency::BitcoinTestnet
            },
        "invoice network mismatch"
    );
    ensure!(
        invoice.amount_milli_satoshis() == Some(policy.amount_msat),
        "invoice amount mismatch"
    );
    if let Some(node) = policy.receiver.ldk() {
        match invoice.description() {
            Bolt11InvoiceDescriptionRef::Hash(h) => ensure!(
                h.0.to_byte_array() == decode_hash(hash)?,
                "invoice request mismatch"
            ),
            _ => anyhow::bail!("invoice needs a description hash"),
        }
        ensure!(
            invoice.expiry_time().as_secs() == u64::from(node.expiry_seconds),
            "invoice expiry mismatch"
        );
    }
    let created = invoice.duration_since_epoch().as_secs();
    ensure!(
        created <= time.saturating_add(SKEW),
        "invoice created in the future"
    );
    let end = created
        .checked_add(invoice.expiry_time().as_secs())
        .context("invoice expiry overflow")?;
    ensure!(
        time < end || (grace > 0 && time <= end.saturating_add(grace)),
        "invoice expired"
    );
    Ok(invoice)
}

fn verify(
    policy: &Policy,
    hash: &str,
    encoded: &str,
    time: u64,
) -> anyhow::Result<(String, String, u64)> {
    ensure!(
        policy.receiver.ldk().is_some(),
        "Lightning x402 requires LDK Server"
    );
    ensure!(encoded.len() <= MAX_PAYMENT_HEADER, "payment too large");
    let payment: Payment = serde_json::from_slice(&STANDARD.decode(encoded)?)?;
    ensure!(payment.x402_version == 2, "unsupported x402 version");
    let accepted = &payment.accepted;
    let mut normalized = accepted.clone();
    if normalized.extra.asset_transfer_method.is_none() {
        normalized.extra.asset_transfer_method = Some("bolt11".into());
    }
    ensure!(
        normalized == policy.requirements(hash.to_owned(), accepted.extra.invoice.clone()),
        "payment terms or request mismatch"
    );
    let invoice = validate_invoice(policy, hash, &accepted.extra.invoice, time, SKEW)?;
    ensure!(
        openssl::sha::sha256(&decode_hash(&payment.payload.preimage)?)
            == invoice.payment_hash().to_byte_array(),
        "invalid payment proof"
    );
    let payment_hash = invoice.payment_hash().to_string();
    let key = format!("{}:{payment_hash}", policy.receiver.network());
    let retain_until = invoice
        .duration_since_epoch()
        .as_secs()
        .saturating_add(invoice.expiry_time().as_secs())
        .saturating_add(SKEW + 3600);
    Ok((key, payment_hash, retain_until))
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Invalid, expired, or already used Lightning payment.")]
    Invalid,
    #[error("Lightning payments temporarily unavailable.")]
    Unavailable,
}

pub struct Lightning {
    path: PathBuf,
    payments: Arc<super::Payments>,
    database: Mutex<Option<Arc<Database>>>,
    // Limit invoice creation independently of free-request quotas, across reloads.
    issuance: crate::rate_limit::RateLimiter,
}
impl Lightning {
    pub fn new(data: &Path, payments: Arc<super::Payments>) -> Self {
        Self {
            path: data.join("lightning").join("settlements.redb"),
            payments,
            database: Mutex::new(None),
            issuance: crate::rate_limit::RateLimiter::new(crate::rate_limit::Limit {
                per_second: 1,
                burst: 3,
            }),
        }
    }
    pub async fn challenge(
        &self,
        policy: &Policy,
        url: &str,
        hash: &str,
        client: std::net::IpAddr,
    ) -> anyhow::Result<Challenge> {
        ensure!(
            self.issuance.check(client).is_ok(),
            "invoice issuance rate exceeded"
        );
        // Establish durable storage before asking anyone to pay.
        self.database().await?;
        let invoice = match &policy.receiver {
            ReceiverConfig::Ldk(node) => receiver::invoice(node, policy.amount_msat, hash).await?,
            ReceiverConfig::Cashu { mint, .. } => self.cashu_invoice(policy, mint, hash).await?,
        };
        let parsed = validate_invoice(policy, hash, &invoice, now()?, 0)?;
        let l402 = if policy.protocols.l402 {
            Some(self.mint_l402(policy, hash, &invoice, &parsed).await?)
        } else {
            None
        };
        let x402 = policy.protocols.x402.then(|| {
            STANDARD.encode(
                json!({
                    "x402Version": 2, "resource": {"url": url},
                    "accepts": [policy.requirements(hash.to_owned(), invoice)]
                })
                .to_string(),
            )
        });
        Ok(Challenge { x402, l402 })
    }
    async fn database(&self) -> anyhow::Result<Arc<Database>> {
        let mut guard = self.database.lock().await;
        if let Some(database) = guard.as_ref() {
            return Ok(database.clone());
        }
        let path = self.path.clone();
        let db = tokio::task::spawn_blocking(move || -> anyhow::Result<Database> {
            let parent = path.parent().context("no storage parent")?;
            super::private_directory(parent)?;
            super::private_file(&path)?;
            let db = Database::create(&path)?;
            let mut tx = db.begin_write()?;
            tx.set_durability(Durability::Immediate)?;
            tx.open_table(SPENT)?;
            tx.open_table(l402::ROOTS)?;
            tx.open_table(cashu::QUOTES)?;
            tx.open_table(cashu::MINTS)?;
            tx.commit()?;
            std::fs::File::open(parent)?.sync_all()?;
            Ok(db)
        })
        .await??;
        let db = Arc::new(db);
        *guard = Some(db.clone());
        Ok(db)
    }
    pub async fn settle(
        &self,
        policy: &Policy,
        hash: &str,
        encoded: &str,
    ) -> Result<String, Error> {
        let time = now().map_err(|_| Error::Unavailable)?;
        let (key, payment_hash, retain_until) =
            verify(policy, hash, encoded, time).map_err(|_| Error::Invalid)?;
        self.claim(key, retain_until).await?;
        let response = json!({"success": true, "transaction": payment_hash, "network": policy.receiver.network()});
        Ok(STANDARD.encode(response.to_string()))
    }

    async fn claim(&self, key: String, retain_until: u64) -> Result<(), Error> {
        let db = self.database().await.map_err(|_| Error::Unavailable)?;
        let claimed = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            let mut tx = db.begin_write()?;
            tx.set_durability(Durability::Immediate)?;
            let claimed = {
                let mut table = tx.open_table(SPENT)?;
                if table.get(key.as_str())?.is_some() {
                    false
                } else {
                    table.insert(key.as_str(), retain_until)?;
                    true
                }
            };
            tx.commit()?;
            Ok(claimed)
        })
        .await
        .map_err(|_| Error::Unavailable)?
        .map_err(|_| Error::Unavailable)?;
        if !claimed {
            return Err(Error::Invalid);
        }
        // Both protocols consume the same network:payment_hash key.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        hashes::sha256,
        secp256k1::{Secp256k1, SecretKey},
    };
    use lightning_invoice::{InvoiceBuilder, PaymentSecret};

    pub(super) fn policy() -> Policy {
        Policy {
            amount_msat: 25000,
            origin: "https://api.example.com".into(),
            headers: vec![],
            protocols: Protocols::default(),
            receiver: ReceiverConfig::Ldk(LdkReceiverConfig {
                endpoint: "https://localhost:3536".into(),
                api_key_file: "/receiver/api_key".into(),
                tls_cert_file: "/receiver/tls.crt".into(),
                pay_to: "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798".into(),
                network: MAINNET.into(),
                expiry_seconds: 300,
            }),
        }
    }
    pub(super) fn signed(policy: &Policy, hash: &str, time: u64) -> String {
        let mut key = [0; 32];
        key[31] = 1;
        let key = SecretKey::from_slice(&key).expect("test key");
        InvoiceBuilder::new(Currency::Bitcoin)
            .amount_milli_satoshis(policy.amount_msat)
            .description_hash(sha256::Hash::from_byte_array(
                decode_hash(hash).expect("hash"),
            ))
            .payment_hash(sha256::Hash::hash(&[42; 32]))
            .payment_secret(PaymentSecret([7; 32]))
            .duration_since_epoch(std::time::Duration::from_secs(time))
            .expiry_time(std::time::Duration::from_secs(u64::from(
                policy.receiver.ldk().expect("LDK").expiry_seconds,
            )))
            .min_final_cltv_expiry_delta(18)
            .build_signed(|msg| Secp256k1::new().sign_ecdsa_recoverable(msg, &key))
            .expect("invoice")
            .to_string()
    }
    pub(super) fn payment(policy: &Policy, hash: &str, time: u64) -> serde_json::Value {
        json!({"x402Version": 2, "accepted": policy.requirements(hash.into(), signed(policy, hash, time)), "payload": {"preimage": hex(&[42; 32])}})
    }
    fn encoded(value: &serde_json::Value) -> String {
        STANDARD.encode(value.to_string())
    }

    #[test]
    fn official_http_vector_and_header_binding() -> anyhow::Result<()> {
        let mut p = policy();
        p.receiver.validate()?;
        let headers = http::HeaderMap::new();
        let a = request_hash(
            &p,
            "GET",
            "https://api.example.com/article/A",
            &headers,
            b"",
        )?;
        assert_eq!(
            a,
            "0d6623f775e025501fa7f0a30b54da25aad62b6ccfe35c85da38016711e6c018"
        );
        assert_eq!(
            request_hash(
                &p,
                "GET",
                "https://api.example.com/article/B",
                &headers,
                b""
            )?,
            "4a99860f75eed1ea8178a5db488e044173bc570c8a6210f2c8590cdf8622d509"
        );
        for (method, body) in [("POST", &b""[..]), ("GET", &b"x"[..])] {
            assert_ne!(
                a,
                request_hash(
                    &p,
                    method,
                    "https://api.example.com/article/A",
                    &headers,
                    body
                )?
            );
        }
        p.headers = vec!["authorization".into()];
        let absent = request_hash(
            &p,
            "GET",
            "https://api.example.com/article/A",
            &headers,
            b"",
        )?;
        let mut headers = headers;
        headers.insert("authorization", http::HeaderValue::from_static(""));
        assert_ne!(
            absent,
            request_hash(
                &p,
                "GET",
                "https://api.example.com/article/A",
                &headers,
                b""
            )?
        );
        Ok(())
    }

    #[test]
    fn proof_checks_terms_signed_binding_preimage_network_and_expiry() -> anyhow::Result<()> {
        let p = policy();
        let hash = digest(b"request");
        let time = 1700000000;
        let value = payment(&p, &hash, time);
        assert!(verify(&p, &hash, &encoded(&value), time).is_ok());
        assert!(verify(&p, &hash, &encoded(&value), time + 360).is_ok());
        assert!(verify(&p, &hash, &encoded(&value), time + 361).is_err());
        assert!(verify(&p, &hash, &encoded(&value), time - 61).is_err());
        for (pointer, bad) in [
            ("/x402Version", json!(1)),
            ("/accepted/amount", json!("24000")),
            ("/accepted/network", json!(TESTNET)),
            ("/accepted/extra/paymentFlow", json!("deferred")),
            ("/accepted/extra/requestBindingProfile", json!("mcp:1")),
            ("/payload/preimage", json!(hex(&[43; 32]))),
            ("/payload/preimage", json!("AA".repeat(32))),
            ("/accepted/extra/assetTransferMethod", json!("other")),
        ] {
            let mut changed = value.clone();
            *changed.pointer_mut(pointer).expect("field") = bad;
            assert!(
                verify(&p, &hash, &encoded(&changed), time).is_err(),
                "{pointer}"
            );
        }
        let other = digest(b"other request");
        let mut changed = value.clone();
        changed["accepted"]["extra"]["requestHash"] = json!(other);
        assert!(verify(&p, &other, &encoded(&changed), time).is_err());
        let mut changed = value;
        changed["accepted"]["extra"]
            .as_object_mut()
            .expect("extra")
            .remove("assetTransferMethod");
        assert!(verify(&p, &hash, &encoded(&changed), time).is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_settlement_is_single_use_across_restart() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let lightning = Lightning::new(
            root.path(),
            Arc::new(super::super::Payments::new(root.path())),
        );
        let p = policy();
        let hash = digest(b"request");
        let value = encoded(&payment(&p, &hash, now()?));
        let (a, b) = tokio::join!(
            lightning.settle(&p, &hash, &value),
            lightning.settle(&p, &hash, &value)
        );
        assert_ne!(a.is_ok(), b.is_ok());
        let receipt: serde_json::Value = serde_json::from_slice(&STANDARD.decode(a.or(b)?)?)?;
        assert_eq!(receipt["network"], MAINNET);
        assert!(receipt.get("payer").is_none());
        drop(lightning);
        let lightning = Lightning::new(
            root.path(),
            Arc::new(super::super::Payments::new(root.path())),
        );
        assert!(matches!(
            lightning.settle(&p, &hash, &value).await,
            Err(Error::Invalid)
        ));
        Ok(())
    }

    fn receiver_block() -> &'static str {
        "lightning node {
            endpoint https://localhost:3536
            api_key_file /receiver/api_key
            tls_cert_file /receiver/tls.crt
            pay_to 0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798
            network mainnet
        }"
    }

    #[test]
    fn named_receivers_are_shared_with_site_specific_bindings_and_forward_references()
    -> anyhow::Result<()> {
        let config = crate::config::Config::parse(&format!(
            "api.example.com {{ respond ok rate_limit 1/s burst 1 lightning_over_limit 25 sat node lightning_headers authorization content-type pay_over_limit 2 sat https://mint.example.com }}
             http://localhost {{ lightning_headers none lightning_origin http://localhost:8080 lightning_over_limit 2 sat node rate_limit 1/s burst 1 respond ok }}
             {}", receiver_block()
        ))?;
        let site = &config.sites["api.example.com"];
        assert!(site.payment.is_some() && site.capacity.is_some());
        let first = site.lightning.as_ref().expect("policy");
        let second = config.sites["localhost"]
            .lightning
            .as_ref()
            .expect("policy");
        assert_eq!(
            first.receiver.ldk().expect("LDK").endpoint,
            second.receiver.ldk().expect("LDK").endpoint
        );
        assert_eq!(first.origin, "https://api.example.com");
        assert_eq!(first.headers, ["authorization", "content-type"]);
        assert_eq!(second.origin, "http://localhost:8080");
        assert!(second.headers.is_empty());
        assert_eq!(first.amount_msat, 25000);
        assert_eq!(second.amount_msat, 2000);
        assert_eq!(first.receiver.ldk().expect("LDK").expiry_seconds, 300);
        let testnet = crate::config::Config::parse(&format!(
            "{} api.example.com {{ respond ok rate_limit 1/s burst 1 lightning_over_limit 1 sat node lightning_headers none }}",
            receiver_block().replace("network mainnet", "network testnet expiry_seconds 600")
        ))?;
        let policy = testnet.sites["api.example.com"]
            .lightning
            .as_ref()
            .expect("testnet");
        assert_eq!(policy.receiver.network(), TESTNET);
        assert_eq!(policy.receiver.ldk().expect("LDK").expiry_seconds, 600);
        Ok(())
    }

    #[test]
    fn lightning_protocol_selection() -> anyhow::Result<()> {
        for (directive, x402, l402) in [
            ("", true, false),
            ("lightning_protocols x402", true, false),
            ("lightning_protocols l402", false, true),
            ("lightning_protocols x402 l402", true, true),
        ] {
            let config = crate::config::Config::parse(&format!(
                "{} api.example.com {{ respond ok rate_limit 1/s burst 1 lightning_over_limit 25 sat node lightning_headers cookie {directive} }}",
                receiver_block()
            ))?;
            let protocols = &config.sites["api.example.com"]
                .lightning
                .as_ref()
                .expect("policy")
                .protocols;
            assert_eq!((protocols.x402, protocols.l402), (x402, l402));
        }
        Ok(())
    }

    #[test]
    fn cashu_receivers_require_l402_and_reject_mixed_backend_settings() -> anyhow::Result<()> {
        let block = "lightning mint { mint https://mint.example.com network mainnet }";
        let site = "api.example.com { respond ok rate_limit 1/s burst 1 lightning_over_limit 8 sat mint lightning_headers cookie lightning_protocols l402 }";
        let config = crate::config::Config::parse(&format!("{site} {block}"))?;
        let policy = config.sites["api.example.com"]
            .lightning
            .as_ref()
            .expect("policy");
        assert!(policy.receiver.mint().is_some());
        assert_eq!(policy.receiver.network(), MAINNET);
        for invalid in [
            block.replace(
                "mint https://mint.example.com",
                "mint http://mint.example.com",
            ),
            block.replace("network mainnet", "network regtest"),
            block.replace("network mainnet", ""),
            block.replace(
                "network mainnet",
                "network mainnet endpoint https://localhost:3536",
            ),
            block.replace("network mainnet", "network mainnet pay_to 00"),
            block.replace("network mainnet", "network mainnet expiry_seconds 300"),
        ] {
            assert!(
                crate::config::Config::parse(&format!("{site} {invalid}")).is_err(),
                "{invalid}"
            );
        }
        for protocols in [
            "",
            "lightning_protocols x402",
            "lightning_protocols x402 l402",
        ] {
            let site = site.replace("lightning_protocols l402", protocols);
            assert!(crate::config::Config::parse(&format!("{site} {block}")).is_err());
        }
        Ok(())
    }

    #[test]
    fn invalid_receiver_definitions_and_site_bindings_are_rejected() {
        let block = receiver_block();
        let site = "api.example.com { respond ok rate_limit 1/s burst 1 lightning_over_limit 25 sat node lightning_headers none }";
        for invalid in [
            block.replace("network mainnet", "network regtest"),
            block.replace("network mainnet", "network mainnet network testnet"),
            block.replace("network mainnet", ""),
            block.replace("network mainnet", "network mainnet unknown value"),
            block.replace("network mainnet", "network mainnet expiry_seconds 0"),
            block.replace("network mainnet", "network mainnet expiry_seconds 86401"),
            block.replace("/receiver/api_key", "relative/key"),
            block.replace("https://localhost:3536", "http://localhost:3536"),
            block.replace("lightning node", "lightning invalid.name"),
            format!("{block} {block}"),
        ] {
            assert!(
                crate::config::Config::parse(&format!("{invalid} {site}")).is_err(),
                "{invalid}"
            );
        }
        for invalid in [
            "lightning_over_limit 25 sat missing lightning_headers none",
            "lightning_over_limit 25 sat node",
            "lightning_headers none",
            "lightning_protocols l402",
            "lightning_over_limit 25 sat node lightning_headers none lightning_protocols",
            "lightning_over_limit 25 sat node lightning_headers none lightning_protocols other",
            "lightning_over_limit 25 sat node lightning_headers none lightning_protocols x402 x402",
            "lightning_over_limit 25 sat node lightning_headers none lightning_protocols l402 lightning_protocols x402",
            "lightning_over_limit 25 sat node lightning_headers authorization lightning_protocols l402",
            "lightning_origin https://api.example.com",
            "lightning_over_limit 0 sat node lightning_headers none",
            "lightning_over_limit 25 msat node lightning_headers none",
            "lightning_over_limit 18446744073709551615 sat node lightning_headers none",
            "lightning_over_limit 25 sat node lightning_headers cookie authorization",
            "lightning_over_limit 25 sat node lightning_headers authorization authorization",
            "lightning_over_limit 25 sat node lightning_headers payment-signature",
            "lightning_over_limit 25 sat node lightning_headers authorization none",
            "lightning_over_limit 25 sat node lightning_headers none lightning_headers none",
            "lightning_over_limit 25 sat node lightning_headers none lightning_origin https://other.example.com",
        ] {
            assert!(
                crate::config::Config::parse(&format!(
                    "{block} api.example.com {{ respond ok rate_limit 1/s burst 1 {invalid} }}"
                ))
                .is_err(),
                "{invalid}"
            );
        }
        assert!(crate::config::Config::parse(&format!("{block} api.example.com {{ respond ok lightning_over_limit 25 sat node lightning_headers none }}")).is_err());
    }

    #[test]
    fn reload_updates_receiver_and_site_settings_together() -> anyhow::Result<()> {
        let text = format!(
            "{} api.example.com {{ respond ok rate_limit 1/s burst 1 lightning_over_limit 25 sat node lightning_headers none }}",
            receiver_block()
        );
        let state = crate::state::State::new(crate::config::Config::parse(&text)?);
        let next = text
            .replace("localhost:3536", "localhost:3537")
            .replace("lightning_headers none", "lightning_headers authorization");
        state.replace_config(crate::config::Config::parse(&next)?);
        let config = state.config.load();
        let policy = config.sites["api.example.com"]
            .lightning
            .as_ref()
            .expect("policy");
        assert_eq!(
            policy.receiver.ldk().expect("LDK").endpoint,
            "https://localhost:3537"
        );
        assert_eq!(policy.headers, ["authorization"]);
        Ok(())
    }
}
