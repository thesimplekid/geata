//! Interactive GET client for Geata's HTTP L402 and Lightning x402 challenges.
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use bitcoin::hashes::{Hash, sha256};
use bytes::Bytes;
use clap::{Parser, ValueEnum};
use http::{Request, StatusCode};
use http_body_util::{BodyExt, Empty, Limited};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use lightning_invoice::Bolt11Invoice;
use serde_json::{Value, json};
use std::{io, io::Write, time::Duration};

#[derive(Clone, Copy, ValueEnum)]
enum Protocol {
    L402,
    X402,
}

#[derive(Parser)]
#[command(about = "GET a URL, pay its Lightning invoice externally, then retry")]
struct Args {
    #[arg(long, value_enum)]
    protocol: Protocol,
    url: String,
    /// Maximum initial GETs, stopping at the first 402 (each may reach the backend).
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=1000))]
    requests: u16,
}

fn missing_challenge(headers: &http::HeaderMap, expected: &str) -> String {
    let advertised = ["www-authenticate", "payment-required", "x-cashu"]
        .into_iter()
        .filter(|name| headers.contains_key(*name))
        .collect::<Vec<_>>();
    let advertised = if advertised.is_empty() {
        "none".to_owned()
    } else {
        advertised.join(", ")
    };
    format!(
        "402 response is missing {expected}. Payment-related headers present: {advertised}. \
         The client --protocol flag does not enable server support. Check the site's \
         lightning_pay and lightning_protocols settings. Geata can return a \
         Cashu-only challenge when Lightning invoice creation or request binding is unavailable."
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let url = url::Url::parse(&args.url)?;
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URL credentials are unsupported"
    );
    ensure!(url.fragment().is_none(), "URL fragments are unsupported");
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "Use HTTPS or loopback HTTP"
    );
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let connector = HttpsConnectorBuilder::new()
        .try_with_platform_verifier()?
        .https_or_http()
        .enable_http1()
        .build();
    let client = Client::builder(TokioExecutor::new()).build::<_, Empty<Bytes>>(connector);
    let request = || Request::get(url.as_str()).header("accept", "*/*");
    let mut challenge = None;
    for _ in 0..args.requests {
        let response = tokio::time::timeout(
            Duration::from_secs(30),
            client.request(request().body(Empty::new())?),
        )
        .await??;
        let status = response.status();
        let headers = response.headers().clone();
        let body = tokio::time::timeout(
            Duration::from_secs(30),
            Limited::new(response.into_body(), 1024 * 1024).collect(),
        )
        .await?
        .map_err(|error| anyhow::anyhow!("reading response: {error}"))?
        .to_bytes();
        println!("{status}");
        if status == StatusCode::PAYMENT_REQUIRED {
            challenge = Some(headers);
            break;
        }
        print!("{}", String::from_utf8_lossy(&body));
        if !body.is_empty() && !body.ends_with(b"\n") {
            println!();
        }
        ensure!(
            status.is_success(),
            "Expected success or 402; redirects are not followed"
        );
    }
    let Some(headers) = challenge else {
        println!(
            "\nNo payment needed. Use --requests to exhaust a free allowance on your test endpoint."
        );
        return Ok(());
    };
    let (invoice, macaroon, accepted) = match args.protocol {
        Protocol::L402 => {
            let header = headers
                .get("www-authenticate")
                .with_context(|| missing_challenge(&headers, "WWW-Authenticate (L402)"))?
                .to_str()?;
            let (macaroon, invoice) = header
                .strip_prefix("L402 macaroon=\"")
                .and_then(|value| value.strip_suffix('"'))
                .and_then(|value| value.split_once("\", invoice=\""))
                .context("Expected Geata's L402 macaroon/invoice challenge")?;
            (invoice.to_owned(), macaroon.to_owned(), Value::Null)
        }
        Protocol::X402 => {
            let header = headers
                .get("payment-required")
                .with_context(|| missing_challenge(&headers, "PAYMENT-REQUIRED (x402)"))?
                .to_str()?;
            let required: Value = serde_json::from_slice(&STANDARD.decode(header)?)?;
            ensure!(required["x402Version"] == 2, "Expected x402 v2");
            let accepted = required["accepts"]
                .as_array()
                .context("Missing accepts")?
                .iter()
                .find(|offer| {
                    offer["scheme"] == "exact"
                        && offer["asset"] == "BTC"
                        && offer["network"]
                            .as_str()
                            .is_some_and(|network| network.starts_with("lnbtc:"))
                        && offer["extra"]["assetTransferMethod"] == "bolt11"
                        && offer["extra"]["paymentFlow"] == "upfront"
                })
                .context("No exact Lightning BOLT11 offer")?
                .clone();
            let invoice = accepted["extra"]["invoice"]
                .as_str()
                .context("Missing invoice")?
                .to_owned();
            (invoice, String::new(), accepted)
        }
    };
    let parsed: Bolt11Invoice = invoice.parse().context("Invalid BOLT11 invoice")?;
    ensure!(!parsed.is_expired(), "Invoice expired");
    let amount = parsed
        .amount_milli_satoshis()
        .context("Invoice has no amount")?;
    println!(
        "\nInvoice: {invoice}\nAmount: {amount} msat\nNetwork: {:?}",
        parsed.currency()
    );
    println!(
        "Review and pay this invoice in your Lightning wallet, then paste its 64-character hex payment preimage."
    );
    print!("Preimage: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let preimage = input.trim().to_ascii_lowercase();
    ensure!(
        preimage.len() == 64 && preimage.bytes().all(|b| b.is_ascii_hexdigit()),
        "Expected 32 bytes of hex"
    );
    let bytes = (0..64)
        .step_by(2)
        .map(|i| u8::from_str_radix(&preimage[i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        &sha256::Hash::hash(&bytes) == parsed.payment_hash(),
        "Preimage does not match invoice"
    );
    let (header, credential) = match args.protocol {
        Protocol::L402 => ("authorization", format!("L402 {macaroon}:{preimage}")),
        Protocol::X402 => (
            "payment-signature",
            STANDARD.encode(serde_json::to_vec(&json!({
                "x402Version": 2, "accepted": accepted, "payload": {"preimage": preimage}
            }))?),
        ),
    };
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        client.request(request().header(header, credential).body(Empty::new())?),
    )
    .await??;
    let status = response.status();
    println!("\nPaid retry: {status}");
    let body = tokio::time::timeout(
        Duration::from_secs(30),
        Limited::new(response.into_body(), 1024 * 1024).collect(),
    )
    .await?
    .map_err(|error| anyhow::anyhow!("reading response: {error}"))?
    .to_bytes();
    io::stdout().write_all(&body)?;
    ensure!(status.is_success(), "Paid retry returned {status}");
    Ok(())
}
