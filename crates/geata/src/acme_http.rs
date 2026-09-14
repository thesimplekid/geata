use std::{future::Future, path::Path, pin::Pin, sync::Arc, time::UNIX_EPOCH};

use bytes::Bytes;
use http::{Request, StatusCode, header::RETRY_AFTER};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use instant_acme::{BodyWrapper, BytesResponse, Error, HttpClient};
use parking_lot::Mutex;
use rustls_pki_types::{CertificateDer, pem::PemObject};

use crate::{certificates::now, storage::Storage};

/// Share a conservative CA-wide cooldown across workers and process restarts.
/// This also covers account/directory requests, which have their own rate limits.
pub struct Cooldown {
    until: Mutex<i64>,
    storage: Arc<Storage>,
}

impl Cooldown {
    pub fn load(storage: Arc<Storage>) -> anyhow::Result<Self> {
        let until = storage.read("ca-retry.json")?.unwrap_or(0);
        Ok(Self {
            until: Mutex::new(until),
            storage,
        })
    }

    pub fn ready(&self) -> bool {
        now() >= *self.until.lock()
    }

    fn defer(&self, deadline: i64) -> anyhow::Result<()> {
        let mut until = self.until.lock();
        *until = (*until).max(deadline);
        // Keep the in-memory barrier even if the disk becomes unavailable.
        self.storage.write("ca-retry.json", &*until)
    }

    pub fn persist(&self) -> anyhow::Result<()> {
        self.storage.write("ca-retry.json", &*self.until.lock())
    }
}

pub struct AcmeHttp {
    client: Client<HttpsConnector<HttpConnector>, BodyWrapper<Bytes>>,
    cooldown: Arc<Cooldown>,
}

impl AcmeHttp {
    pub fn new(root: Option<&Path>, cooldown: Arc<Cooldown>) -> anyhow::Result<Self> {
        let builder = HttpsConnectorBuilder::new();
        let builder = if let Some(path) = root {
            let mut roots = rustls::RootCertStore::empty();
            roots.add(CertificateDer::from_pem_file(path)?)?;
            builder.with_tls_config(
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        } else {
            builder.try_with_platform_verifier()?
        };
        let connector = builder.https_only().enable_http1().enable_http2().build();
        Ok(Self {
            client: Client::builder(TokioExecutor::new()).build(connector),
            cooldown,
        })
    }
}

impl HttpClient for AcmeHttp {
    fn request(
        &self,
        request: Request<BodyWrapper<Bytes>>,
    ) -> Pin<Box<dyn Future<Output = Result<BytesResponse, Error>> + Send>> {
        let client = self.client.clone();
        let cooldown = self.cooldown.clone();
        Box::pin(async move {
            if !cooldown.ready() {
                return Err(Error::Str("CA Retry-After cooldown is active"));
            }
            let response =
                tokio::time::timeout(std::time::Duration::from_secs(30), client.request(request))
                    .await
                    .map_err(|_| Error::Str("ACME HTTP request timed out"))?
                    .map_err(|e| Error::Other(Box::new(e)))?;
            // Round up the wall clock so a relative Retry-After never loses its
            // fractional second when persisted as an integer Unix deadline.
            if let Some(deadline) = retry_deadline(
                response.status(),
                response.headers(),
                now().saturating_add(1),
            ) {
                if let Err(error) = cooldown.defer(deadline) {
                    tracing::error!(%error, "cannot persist CA cooldown; issuance will pause until storage recovers");
                }
                tracing::warn!(
                    retry_at = deadline,
                    "CA requested a pause in certificate requests"
                );
            }
            Ok(response.into())
        })
    }
}

fn retry_deadline(status: StatusCode, headers: &http::HeaderMap, now: i64) -> Option<i64> {
    if !status.is_client_error() && !status.is_server_error() {
        return None; // Successful order polling stays under instant-acme's retry policy.
    }
    let header = headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim);
    let parsed = header.and_then(|v| {
        v.parse::<u64>()
            .ok()
            .map(|seconds| now.saturating_add(seconds.min(i64::MAX as u64) as i64))
            .or_else(|| {
                httpdate::parse_http_date(v)
                    .ok()?
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs().min(i64::MAX as u64) as i64)
            })
    });
    parsed
        .map(|deadline| deadline.max(now.saturating_add(1)))
        .or_else(|| (status == StatusCode::TOO_MANY_REQUESTS).then(|| now.saturating_add(60)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_seconds_dates_and_rate_limit_fallback() {
        let mut headers = http::HeaderMap::new();
        headers.insert(RETRY_AFTER, "120".parse().expect("header"));
        assert_eq!(
            retry_deadline(StatusCode::TOO_MANY_REQUESTS, &headers, 1000),
            Some(1120)
        );
        assert_eq!(retry_deadline(StatusCode::OK, &headers, 1000), None);
        headers.insert(
            RETRY_AFTER,
            "Thu, 01 Jan 1970 01:00:00 GMT".parse().expect("header"),
        );
        assert_eq!(
            retry_deadline(StatusCode::SERVICE_UNAVAILABLE, &headers, 1000),
            Some(3600)
        );
        headers.insert(RETRY_AFTER, "invalid".parse().expect("header"));
        assert_eq!(
            retry_deadline(StatusCode::TOO_MANY_REQUESTS, &headers, 1000),
            Some(1060)
        );
        assert_eq!(
            retry_deadline(StatusCode::SERVICE_UNAVAILABLE, &headers, 1000),
            None
        );
    }

    #[test]
    fn ca_cooldown_survives_restart_and_storage_failure() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("state");
        let storage = Arc::new(Storage::open(&path, "https://ca.test")?);
        let cooldown = Cooldown::load(storage.clone())?;
        let deadline = now() + 1000;
        cooldown.defer(deadline)?;
        cooldown.defer(deadline - 500)?;
        drop(cooldown);
        drop(storage);
        let storage = Arc::new(Storage::open(&path, "https://ca.test")?);
        let cooldown = Cooldown::load(storage)?;
        assert!(!cooldown.ready());
        assert_eq!(*cooldown.until.lock(), deadline);
        // A real filesystem failure must not clear the in-memory pause.
        std::fs::rename(&path, root.path().join("unavailable"))?;
        assert!(cooldown.defer(deadline + 500).is_err());
        assert!(!cooldown.ready());
        Ok(())
    }
}
