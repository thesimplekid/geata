use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, ensure};
use async_trait::async_trait;
use openssl::{
    asn1::{Asn1Time, Asn1TimeRef},
    pkey::{PKey, Private},
    x509::X509,
};
use serde::{Deserialize, Serialize};

use crate::state::State;

#[derive(Serialize, Deserialize)]
pub struct CertificatePem {
    pub chain: String,
    pub private_key: String,
}

pub struct Certificate {
    chain: Vec<X509>,
    key: PKey<Private>,
    not_before: i64,
    not_after: i64,
}

impl Certificate {
    pub fn parse(domain: &str, pem: &CertificatePem) -> anyhow::Result<Self> {
        let chain = X509::stack_from_pem(pem.chain.as_bytes())?;
        let leaf = chain.first().context("certificate chain is empty")?;
        let key = PKey::private_key_from_pem(pem.private_key.as_bytes())?;
        ensure!(
            leaf.public_key()?.public_eq(&key),
            "certificate and private key do not match"
        );
        ensure!(
            leaf.subject_alt_names()
                .is_some_and(|names| names.iter().any(|name| name
                    .dnsname()
                    .is_some_and(|name| name.eq_ignore_ascii_case(domain)))),
            "certificate does not cover {domain}"
        );
        let not_before = timestamp(leaf.not_before())?;
        let not_after = timestamp(leaf.not_after())?;
        ensure!(
            not_after > not_before,
            "invalid certificate validity period"
        );
        Ok(Self {
            chain,
            key,
            not_before,
            not_after,
        })
    }

    pub fn valid_at(&self, now: i64) -> bool {
        now >= self.not_before && now < self.not_after
    }

    pub fn needs_renewal(&self, now: i64) -> bool {
        // Works for both conventional and short-lived certificates.
        !self.valid_at(now) || now >= self.not_before + (self.not_after - self.not_before) * 2 / 3
    }

    pub fn expires_at(&self) -> i64 {
        self.not_after
    }
}

fn timestamp(time: &Asn1TimeRef) -> anyhow::Result<i64> {
    let delta = Asn1Time::from_unix(0)?.diff(time)?;
    Ok(i64::from(delta.days) * 86400 + i64::from(delta.secs))
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct DynamicTls(pub Arc<State>);

#[async_trait]
impl pingora::listeners::TlsAccept for DynamicTls {
    async fn certificate_callback(&self, ssl: &mut pingora::tls::ssl::SslRef) {
        use pingora::tls::{ext, ssl::NameType};
        let Some(domain) = ssl
            .servername(NameType::HOST_NAME)
            .map(str::to_ascii_lowercase)
        else {
            return;
        };
        if !self
            .0
            .config
            .load()
            .sites
            .get(&domain)
            .is_some_and(|s| s.https)
        {
            return;
        }
        let Some(cert) = self.0.certificates.read().get(&domain).cloned() else {
            return;
        };
        if !cert.valid_at(now()) {
            return;
        }
        let mut install = || -> Result<(), openssl::error::ErrorStack> {
            ext::ssl_use_certificate(ssl, &cert.chain[0])?;
            ext::ssl_use_private_key(ssl, &cert.key)?;
            for intermediate in &cert.chain[1..] {
                ext::ssl_add_chain_cert(ssl, intermediate)?;
            }
            Ok(())
        };
        if let Err(error) = (install)() {
            tracing::error!(%domain, %error, "cannot install TLS certificate");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use openssl::{
        bn::BigNum,
        hash::MessageDigest,
        rsa::Rsa,
        x509::{X509NameBuilder, extension::SubjectAlternativeName},
    };

    pub fn fixture(domain: &str, start: i64, end: i64) -> anyhow::Result<CertificatePem> {
        let key = PKey::from_rsa(Rsa::generate(2048)?)?;
        let mut name = X509NameBuilder::new()?;
        name.append_entry_by_text("CN", domain)?;
        let name = name.build();
        let mut builder = X509::builder()?;
        builder.set_version(2)?;
        let serial = BigNum::from_u32(1)?.to_asn1_integer()?;
        builder.set_serial_number(&serial)?;
        builder.set_subject_name(&name)?;
        builder.set_issuer_name(&name)?;
        builder.set_pubkey(&key)?;
        let start = Asn1Time::from_unix(start)?;
        let end = Asn1Time::from_unix(end)?;
        builder.set_not_before(&start)?;
        builder.set_not_after(&end)?;
        let san = SubjectAlternativeName::new()
            .dns(domain)
            .build(&builder.x509v3_context(None, None))?;
        builder.append_extension(san)?;
        builder.sign(&key, MessageDigest::sha256())?;
        Ok(CertificatePem {
            chain: String::from_utf8(builder.build().to_pem()?)?,
            private_key: String::from_utf8(key.private_key_to_pem_pkcs8()?)?,
        })
    }

    #[test]
    fn renewal_tracks_lifetime_and_rejects_wrong_identity() -> anyhow::Result<()> {
        let pem = fixture("example.com", 1000, 10000)?;
        let cert = Certificate::parse("example.com", &pem)?;
        assert!(!cert.valid_at(999));
        assert!(cert.valid_at(1000));
        assert!(!cert.valid_at(10000));
        assert!(!cert.needs_renewal(6999));
        assert!(cert.needs_renewal(7000));
        assert!(Certificate::parse("other.example.com", &pem).is_err());
        let other = fixture("example.com", 1000, 10000)?;
        assert!(
            Certificate::parse(
                "example.com",
                &CertificatePem {
                    chain: pem.chain,
                    private_key: other.private_key
                }
            )
            .is_err()
        );
        Ok(())
    }
}
