//! HTTP L402 credentials with first-party restrictions and shared single-use admission.
use anyhow::{Context, ensure};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE},
};
use bitcoin::hashes::Hash;
use lightning_invoice::Bolt11Invoice;
use macaroon::{Caveat, Format, Macaroon, MacaroonKey, Verifier};
use openssl::{hash::MessageDigest, pkey::PKey, sign::Signer};
use redb::{Durability, ReadableDatabase, ReadableTable, TableDefinition};

use super::{Error, Lightning, MAX_PAYMENT_HEADER, Policy, SKEW, decode_hash, digest, now};

// Each credential has a distinct random root key, durably stored before advertising.
// Expiration is exclusive. Keys are retained with the journal across restarts.
pub(super) const ROOTS: TableDefinition<&str, (&[u8], u64)> = TableDefinition::new("l402_roots_v1");

fn terms(policy: &Policy, hash: &str) -> anyhow::Result<String> {
    let requirements = policy.requirements(hash.into(), String::new());
    let encoded = if let Some(mint) = policy.receiver.mint() {
        serde_json::to_vec(&serde_json::json!({"terms": requirements, "mint": mint}))?
    } else {
        serde_json::to_vec(&requirements)?
    };
    Ok(digest(&encoded))
}

fn hmac(key: &[u8], bytes: &[u8]) -> anyhow::Result<[u8; 32]> {
    let key = PKey::hmac(key)?;
    let mut signer = Signer::new(MessageDigest::sha256(), &key)?;
    signer.update(bytes)?;
    signer
        .sign_to_vec()?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid HMAC length"))
}

fn parse_credential(encoded: &str) -> anyhow::Result<(Macaroon, [u8; 32])> {
    ensure!(
        encoded.len() <= MAX_PAYMENT_HEADER,
        "L402 credential too large"
    );
    let (token, preimage) = encoded.split_once(':').context("missing L402 proof")?;
    let bytes = STANDARD.decode(token)?;
    ensure!(bytes.first() == Some(&2), "L402 requires a V2 macaroon");
    let macaroon = Macaroon::deserialize_binary(&bytes)?;
    ensure!(
        macaroon.caveats().len() <= 32 && macaroon.third_party_caveats().is_empty(),
        "unsupported macaroon caveats"
    );
    let id = macaroon.identifier();
    ensure!(
        id.0.len() == 66 && id.0[..2] == [0, 0],
        "invalid L402 identifier"
    );
    let preimage = decode_hash(&preimage.to_ascii_lowercase())?;
    ensure!(
        openssl::sha::sha256(&preimage) == id.0[2..34],
        "invalid payment proof"
    );
    Ok((macaroon, preimage))
}

fn verify(
    policy: &Policy,
    hash: &str,
    macaroon: &Macaroon,
    preimage: &[u8; 32],
    root: &[u8; 32],
    valid_before: u64,
    time: u64,
) -> anyhow::Result<()> {
    ensure!(time < valid_before, "expired credential");
    let expected_request = format!("geata_request={hash}");
    let expected_terms = format!("geata_terms={}", terms(policy, hash)?);
    let mut found = [false; 4];
    let mut expiry = valid_before;
    let mut signature = hmac(root, &macaroon.identifier().0)?;
    let mut verifier = Verifier::default();
    for caveat in macaroon.caveats() {
        let Caveat::FirstParty(caveat) = caveat else {
            anyhow::bail!("third-party caveats unsupported");
        };
        let predicate = caveat.predicate();
        signature = hmac(&signature, &predicate.0)?;
        let text = std::str::from_utf8(&predicate.0)?;
        if text == "services=geata:0" {
            found[0] = true;
        } else if text == expected_request {
            found[1] = true;
        } else if text == expected_terms {
            found[2] = true;
        } else if let Some(value) = text.strip_prefix("geata_valid_until=") {
            ensure!(
                !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
                "invalid expiration"
            );
            let next: u64 = value.parse()?;
            ensure!(
                next <= expiry && time < next,
                "expiration must restrict access"
            );
            expiry = next;
            found[3] = true;
        } else if let Some(value) = text.strip_prefix("preimage=") {
            ensure!(
                decode_hash(&value.to_ascii_lowercase())? == *preimage,
                "preimage caveat mismatch"
            );
        } else {
            anyhow::bail!("unsupported or unsatisfied caveat");
        }
        verifier.satisfy_exact(predicate);
    }
    ensure!(
        found.into_iter().all(|present| present),
        "missing required restriction"
    );
    // The library's equality comparison is not constant-time. Check the full
    // first-party chain with OpenSSL first, before invoking its verifier.
    ensure!(
        openssl::memcmp::eq(&signature, &macaroon.signature()[..]),
        "invalid signature"
    );
    verifier.verify(macaroon, &MacaroonKey::from(*root), Vec::new())?;
    Ok(())
}

impl Lightning {
    pub(super) async fn mint_l402(
        &self,
        policy: &Policy,
        hash: &str,
        invoice: &str,
        parsed: &Bolt11Invoice,
    ) -> anyhow::Result<String> {
        macaroon::initialize()?;
        let mut root = [0; 32];
        openssl::rand::rand_bytes(&mut root)?;
        let mut identifier = vec![0; 66];
        identifier[2..34].copy_from_slice(&parsed.payment_hash().to_byte_array());
        openssl::rand::rand_bytes(&mut identifier[34..])?;
        let key = digest(&identifier);
        let valid_before = parsed
            .duration_since_epoch()
            .as_secs()
            .checked_add(parsed.expiry_time().as_secs())
            .and_then(|end| end.checked_add(SKEW + 1))
            .context("expiration overflow")?;
        let predicates = [
            "services=geata:0".to_owned(),
            format!("geata_request={hash}"),
            format!("geata_terms={}", terms(policy, hash)?),
            format!("geata_valid_until={valid_before}"),
        ];
        // Construct through V2JSON to avoid the library's minting debug logs,
        // which include bearer credentials. Cryptography uses OpenSSL HMAC.
        let mut signature = hmac(&root, &identifier)?;
        for predicate in &predicates {
            signature = hmac(&signature, predicate.as_bytes())?;
        }
        let macaroon = Macaroon::deserialize(
            serde_json::json!({
                "v": 2, "i64": STANDARD.encode(&identifier),
                "c": predicates.iter().map(|p| serde_json::json!({"i": p})).collect::<Vec<_>>(),
                "s64": URL_SAFE.encode(signature)
            })
            .to_string(),
        )?;
        let encoded = STANDARD.encode(URL_SAFE.decode(macaroon.serialize(Format::V2)?)?);
        let database = self.database().await?;
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut tx = database.begin_write()?;
            tx.set_durability(Durability::Immediate)?;
            {
                let mut table = tx.open_table(ROOTS)?;
                ensure!(
                    table.get(key.as_str())?.is_none(),
                    "duplicate credential identifier"
                );
                table.insert(key.as_str(), (root.as_slice(), valid_before))?;
            }
            tx.commit()?;
            Ok(())
        })
        .await??;
        Ok(format!(
            "L402 macaroon=\"{encoded}\", invoice=\"{invoice}\""
        ))
    }

    pub async fn settle_l402(
        &self,
        policy: &Policy,
        hash: &str,
        encoded: &str,
    ) -> Result<(), Error> {
        macaroon::initialize().map_err(|_| Error::Unavailable)?;
        let (macaroon, preimage) = parse_credential(encoded).map_err(|_| Error::Invalid)?;
        let identifier = macaroon.identifier();
        let id = digest(&identifier.0);
        let db = self.database().await.map_err(|_| Error::Unavailable)?;
        let record =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Option<([u8; 32], u64)>> {
                let tx = db.begin_read()?;
                let table = tx.open_table(ROOTS)?;
                table
                    .get(id.as_str())?
                    .map(|record| {
                        let (key, expiry) = record.value();
                        Ok((key.try_into().context("invalid root key")?, expiry))
                    })
                    .transpose()
            })
            .await
            .map_err(|_| Error::Unavailable)?
            .map_err(|_| Error::Unavailable)?;
        let (root, valid_before) = record.ok_or(Error::Invalid)?;
        let time = now().map_err(|_| Error::Unavailable)?;
        verify(
            policy,
            hash,
            &macaroon,
            &preimage,
            &root,
            valid_before,
            time,
        )
        .map_err(|_| Error::Invalid)?;
        let key = format!(
            "{}:{}",
            policy.receiver.network(),
            super::hex(&identifier.0[2..34])
        );
        if let Some(mint) = policy.receiver.mint() {
            self.collect_cashu_invoice(policy, mint, &key).await?;
        }
        self.claim(key, valid_before.saturating_add(3599)).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        hex,
        tests::{payment, policy, signed},
    };
    use super::*;

    fn credential(challenge: &str) -> String {
        let token = challenge
            .strip_prefix("L402 macaroon=\"")
            .expect("challenge")
            .split('"')
            .next()
            .expect("macaroon");
        format!("{token}:{}", hex(&[42; 32]))
    }

    #[tokio::test]
    async fn credentials_survive_restart_and_share_atomic_settlement() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let p = policy();
        let hash = digest(b"request");
        let time = now()?;
        let invoice = signed(&p, &hash, time);
        let lightning = Lightning::new(
            root.path(),
            std::sync::Arc::new(crate::payments::Payments::new(root.path())),
        );
        let credential = credential(
            &lightning
                .mint_l402(&p, &hash, &invoice, &invoice.parse()?)
                .await?,
        );
        drop(lightning);
        let lightning = Lightning::new(
            root.path(),
            std::sync::Arc::new(crate::payments::Payments::new(root.path())),
        );
        let x402 = STANDARD.encode(payment(&p, &hash, time).to_string());
        let (l402, x402) = tokio::join!(
            lightning.settle_l402(&p, &hash, &credential),
            lightning.settle(&p, &hash, &x402)
        );
        assert_ne!(l402.is_ok(), x402.is_ok());
        drop(lightning);
        let lightning = Lightning::new(
            root.path(),
            std::sync::Arc::new(crate::payments::Payments::new(root.path())),
        );
        assert!(matches!(
            lightning.settle_l402(&p, &hash, &credential).await,
            Err(Error::Invalid)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn restrictions_and_attenuation_are_enforced() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let lightning = Lightning::new(
            root.path(),
            std::sync::Arc::new(crate::payments::Payments::new(root.path())),
        );
        let p = policy();
        let hash = digest(b"request");
        let invoice = signed(&p, &hash, now()?);
        let credential = credential(
            &lightning
                .mint_l402(&p, &hash, &invoice, &invoice.parse()?)
                .await?,
        );
        let (token, preimage) = parse_credential(&credential)?;
        let db = lightning.database().await?;
        let tx = db.begin_read()?;
        let table = tx.open_table(ROOTS)?;
        let record = table
            .get(digest(&token.identifier().0).as_str())?
            .expect("root");
        let (key, expiry) = record.value();
        let key: [u8; 32] = key.try_into()?;
        let time = now()?;
        verify(&p, &hash, &token, &preimage, &key, expiry, time)?;
        assert!(verify(&p, &hash, &token, &preimage, &key, expiry, expiry).is_err());
        assert!(
            verify(
                &p,
                &digest(b"different"),
                &token,
                &preimage,
                &key,
                expiry,
                time
            )
            .is_err()
        );
        let mut changed = p.clone();
        changed.amount_msat += 1000;
        assert!(verify(&changed, &hash, &token, &preimage, &key, expiry, time).is_err());
        for predicate in [
            "unknown=restriction".to_owned(),
            format!("geata_valid_until={}", expiry + 1),
            format!("geata_valid_until={time}"),
        ] {
            let mut restricted = token.clone();
            restricted.add_first_party_caveat(predicate.into());
            assert!(verify(&p, &hash, &restricted, &preimage, &key, expiry, time).is_err());
        }
        let mut restricted = token.clone();
        restricted.add_first_party_caveat(format!("geata_valid_until={}", time + 10).into());
        restricted.add_first_party_caveat(format!("preimage={}", hex(&preimage)).into());
        verify(&p, &hash, &restricted, &preimage, &key, expiry, time)?;
        let mut altered = STANDARD.decode(credential.split_once(':').expect("proof").0)?;
        let last = altered.len() - 1;
        altered[last] ^= 1;
        let encoded = format!("{}:{}", STANDARD.encode(altered), hex(&preimage));
        assert!(matches!(
            lightning.settle_l402(&p, &hash, &encoded).await,
            Err(Error::Invalid)
        ));
        assert!(
            parse_credential(&format!(
                "{}:{}",
                credential.split_once(':').expect("proof").0,
                hex(&[0; 32])
            ))
            .is_err()
        );
        lightning.settle_l402(&p, &hash, &credential).await?;
        Ok(())
    }
}
