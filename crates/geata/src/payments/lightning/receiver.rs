//! Invoice-only adapter for LDK Server's macaroon-authenticated client.
use std::{io::Read, path::Path, time::Duration};

use anyhow::{Context, ensure};
use ldk_server_client::{
    client::LdkServerClient,
    ldk_server_grpc::{
        api::Bolt11ReceiveRequest,
        types::{Bolt11InvoiceDescription, bolt11_invoice_description},
    },
};

use super::LdkReceiverConfig;

fn read_bounded(path: &Path, max: usize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((max + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= max, "receiver credential file too large");
    Ok(bytes)
}

pub(super) async fn invoice(
    config: &LdkReceiverConfig,
    amount: u64,
    hash: &str,
) -> anyhow::Result<String> {
    let credentials = config.clone();
    let client = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let token = String::from_utf8(read_bounded(
            &credentials.macaroon_file,
            ldk_server_client::macaroon::MAX_MACAROON_BYTES * 2 + 2,
        )?)
        .context("ldk-server macaroon file must contain UTF-8 hex")?;
        let cert = read_bounded(&credentials.tls_cert_file, 1024 * 1024)?;
        // The upstream client expects the authority without the HTTPS scheme.
        let endpoint = url::Url::parse(&credentials.endpoint)?;
        LdkServerClient::new(
            endpoint[url::Position::BeforeHost..url::Position::AfterPort].to_owned(),
            token.trim().to_owned(),
            &cert,
        )
        .map_err(anyhow::Error::msg)
    })
    .await??;
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        client.bolt11_receive(Bolt11ReceiveRequest {
            amount_msat: Some(amount),
            description: Some(Bolt11InvoiceDescription {
                kind: Some(bolt11_invoice_description::Kind::Hash(hash.to_owned())),
            }),
            expiry_secs: config.expiry_seconds,
        }),
    )
    .await
    .context("receiver timed out")??;
    ensure!(!response.invoice.is_empty(), "receiver omitted invoice");
    Ok(response.invoice)
}
