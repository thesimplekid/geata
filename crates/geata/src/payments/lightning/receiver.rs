//! Minimal invoice-only client for ldk-server's api.LightningNode gRPC service.
//! Wire definitions: lightningdevkit/ldk-server, api.proto and types.proto.
use std::{io::Read, path::Path, time::Duration};

use anyhow::{Context, ensure};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use openssl::{hash::MessageDigest, pkey::PKey, sign::Signer};
use prost::Message;

use super::{ReceiverConfig, hex, now};

#[derive(Clone, PartialEq, Message)]
struct ReceiveRequest {
    #[prost(uint64, optional, tag = "1")]
    amount_msat: Option<u64>,
    #[prost(message, optional, tag = "2")]
    description: Option<Description>,
    #[prost(uint32, tag = "3")]
    expiry_secs: u32,
}

#[derive(Clone, PartialEq, Message)]
struct Description {
    // Only the hash variant of the upstream oneof is used.
    #[prost(string, optional, tag = "2")]
    hash: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ReceiveResponse {
    #[prost(string, tag = "1")]
    invoice: String,
}

fn read_bounded(path: &Path, max: usize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((max + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= max, "receiver credential file too large");
    Ok(bytes)
}

pub(super) async fn invoice(
    config: &ReceiverConfig,
    amount: u64,
    hash: &str,
) -> anyhow::Result<String> {
    let credentials = config.clone();
    let (key, cert) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let key = read_bounded(&credentials.api_key_file, 32)?;
        ensure!(
            key.len() == 32,
            "ldk-server API key must contain 32 raw bytes"
        );
        Ok((key, read_bounded(&credentials.tls_cert_file, 1024 * 1024)?))
    })
    .await??;
    // ldk-server authenticates with the ASCII lower-hex representation of its raw key.
    let key = hex(&key);
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut cert.as_slice()) {
        roots.add(cert?)?;
    }
    ensure!(!roots.is_empty(), "empty ldk-server certificate bundle");
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_only()
        .enable_http2()
        .build();
    let client = Client::builder(TokioExecutor::new()).build::<_, Full<Bytes>>(connector);
    let message = ReceiveRequest {
        amount_msat: Some(amount),
        description: Some(Description {
            hash: Some(hash.to_owned()),
        }),
        expiry_secs: config.expiry_seconds,
    }
    .encode_to_vec();
    let mut frame = vec![0];
    frame.extend_from_slice(&u32::try_from(message.len())?.to_be_bytes());
    frame.extend_from_slice(&message);
    let timestamp = now()?;
    let auth = authentication(&key, timestamp, &frame)?;
    let request = http::Request::post(format!(
        "{}/api.LightningNode/Bolt11Receive",
        config.endpoint.trim_end_matches('/')
    ))
    .version(http::Version::HTTP_2)
    .header("content-type", "application/grpc+proto")
    .header("te", "trailers")
    .header("x-auth", auth)
    .body(Full::new(Bytes::from(frame)))?;
    tokio::time::timeout(Duration::from_secs(10), async {
        let response = client.request(request).await?;
        ensure!(response.status().is_success(), "receiver HTTP error");
        let mut status = response.headers().get("grpc-status").cloned();
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame?;
            if let Some(data) = frame.data_ref() {
                ensure!(
                    bytes.len() + data.len() <= 64 * 1024,
                    "receiver response too large"
                );
                bytes.extend_from_slice(data);
            }
            if let Some(trailers) = frame.trailers_ref() {
                status = trailers.get("grpc-status").cloned().or(status);
            }
        }
        ensure!(
            status.as_ref().is_some_and(|s| s == "0"),
            "receiver gRPC error"
        );
        ensure!(bytes.len() >= 5 && bytes[0] == 0, "invalid gRPC frame");
        let length = u32::from_be_bytes(bytes[1..5].try_into()?) as usize;
        ensure!(bytes.len() == length + 5, "invalid gRPC response length");
        let response = ReceiveResponse::decode(&bytes[5..])?;
        ensure!(!response.invoice.is_empty(), "receiver omitted invoice");
        Ok(response.invoice)
    })
    .await
    .context("receiver timed out")?
}

fn authentication(key: &str, timestamp: u64, frame: &[u8]) -> anyhow::Result<String> {
    let key = PKey::hmac(key.as_bytes())?;
    let mut signer = Signer::new(MessageDigest::sha256(), &key)?;
    signer.update(&timestamp.to_be_bytes())?;
    signer.update(frame)?;
    Ok(format!("HMAC {timestamp}:{}", hex(&signer.sign_to_vec()?)))
}
