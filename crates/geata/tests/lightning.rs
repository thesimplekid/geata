//! Local ldk-server gRPC fixture and executable x402 admission tests; no real funds.
use base64::{Engine, engine::general_purpose::STANDARD};
use bitcoin::{
    hashes::{Hash, sha256},
    secp256k1::{Secp256k1, SecretKey},
};
use bytes::Bytes;
use lightning_invoice::{InvoiceBuilder, PaymentSecret};
use openssl::{
    asn1::Asn1Time,
    bn::BigNum,
    hash::MessageDigest,
    pkey::PKey,
    rsa::Rsa,
    sign::Signer,
    x509::{
        X509, X509NameBuilder,
        extension::{BasicConstraints, SubjectAlternativeName},
    },
};
use prost::Message;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, PartialEq, Message)]
struct Receive {
    #[prost(uint64, optional, tag = "1")]
    amount: Option<u64>,
    #[prost(message, optional, tag = "2")]
    description: Option<Description>,
    #[prost(uint32, tag = "3")]
    expiry: u32,
}
#[derive(Clone, PartialEq, Message)]
struct Description {
    #[prost(string, tag = "2")]
    hash: String,
}
#[derive(Clone, PartialEq, Message)]
struct Invoice {
    #[prost(string, tag = "1")]
    invoice: String,
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn certificate() -> anyhow::Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let key = PKey::from_rsa(Rsa::generate(2048)?)?;
    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_text("CN", "localhost")?;
    let name = name.build();
    let mut cert = X509::builder()?;
    cert.set_version(2)?;
    let serial = BigNum::from_u32(1)?.to_asn1_integer()?;
    cert.set_serial_number(&serial)?;
    cert.set_subject_name(&name)?;
    cert.set_issuer_name(&name)?;
    cert.set_pubkey(&key)?;
    let before = Asn1Time::days_from_now(0)?;
    let after = Asn1Time::days_from_now(2)?;
    cert.set_not_before(&before)?;
    cert.set_not_after(&after)?;
    cert.append_extension(BasicConstraints::new().critical().build()?)?;
    let san = SubjectAlternativeName::new()
        .dns("localhost")
        .ip("127.0.0.1")
        .build(&cert.x509v3_context(None, None))?;
    cert.append_extension(san)?;
    cert.sign(&key, MessageDigest::sha256())?;
    let cert = cert.build();
    Ok((cert.to_pem()?, cert.to_der()?, key.private_key_to_pkcs8()?))
}

async fn receiver(
    count: Arc<AtomicUsize>,
    cert: Vec<u8>,
    key: Vec<u8>,
) -> anyhow::Result<(u16, tokio::task::JoinHandle<()>)> {
    use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(cert)],
        PrivatePkcs8KeyDer::from(key).into(),
    )?;
    tls.alpn_protocols = vec![b"h2".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let count = count.clone();
            tokio::spawn(async move {
                let socket = acceptor.accept(socket).await.expect("TLS handshake");
                let mut connection = h2::server::handshake(socket).await.expect("h2");
                while let Some(Ok((request, mut respond))) = connection.accept().await {
                    let count = count.clone();
                    tokio::spawn(async move {
                        assert_eq!(request.uri().path(), "/api.LightningNode/Bolt11Receive");
                        let auth = request.headers()["x-auth"]
                            .to_str()
                            .expect("auth")
                            .to_owned();
                        let mut body = request.into_body();
                        let mut bytes = Vec::new();
                        while let Some(data) = body.data().await {
                            let data = data.expect("body");
                            body.flow_control()
                                .release_capacity(data.len())
                                .expect("capacity");
                            bytes.extend_from_slice(&data);
                        }
                        let (stamp, mac) = auth
                            .strip_prefix("HMAC ")
                            .expect("HMAC")
                            .split_once(':')
                            .expect("timestamp");
                        let stamp: u64 = stamp.parse().expect("timestamp");
                        assert!(seconds().abs_diff(stamp) < 10);
                        let key = PKey::hmac(hex(&[99; 32]).as_bytes()).expect("key");
                        let mut signer =
                            Signer::new(MessageDigest::sha256(), &key).expect("signer");
                        signer.update(&stamp.to_be_bytes()).expect("stamp");
                        signer.update(&bytes).expect("frame");
                        assert_eq!(mac, hex(&signer.sign_to_vec().expect("mac")));
                        assert_eq!(bytes[0], 0);
                        assert_eq!(
                            u32::from_be_bytes(bytes[1..5].try_into().expect("length")) as usize,
                            bytes.len() - 5
                        );
                        let request = Receive::decode(&bytes[5..]).expect("protobuf");
                        let hash: sha256::Hash = request
                            .description
                            .expect("description hash")
                            .hash
                            .parse()
                            .expect("hash");
                        let index = count.fetch_add(1, Ordering::SeqCst) + 1;
                        let preimage = [index as u8; 32];
                        let mut key = [0; 32];
                        key[31] = 1;
                        let invoice = InvoiceBuilder::new(lightning_invoice::Currency::Bitcoin)
                            .amount_milli_satoshis(request.amount.expect("amount"))
                            .description_hash(hash)
                            .payment_hash(sha256::Hash::hash(&preimage))
                            .payment_secret(PaymentSecret([9; 32]))
                            .duration_since_epoch(Duration::from_secs(seconds()))
                            .expiry_time(Duration::from_secs(request.expiry.into()))
                            .min_final_cltv_expiry_delta(18)
                            .build_signed(|msg| {
                                Secp256k1::new().sign_ecdsa_recoverable(
                                    msg,
                                    &SecretKey::from_slice(&key).expect("key"),
                                )
                            })
                            .expect("invoice");
                        let proto = Invoice {
                            invoice: invoice.to_string(),
                        }
                        .encode_to_vec();
                        let mut frame = vec![0];
                        frame.extend_from_slice(&(proto.len() as u32).to_be_bytes());
                        frame.extend_from_slice(&proto);
                        let response = http::Response::builder()
                            .status(200)
                            .header("content-type", "application/grpc+proto")
                            .body(())
                            .expect("response");
                        let mut stream = respond.send_response(response, false).expect("headers");
                        stream.send_data(Bytes::from(frame), false).expect("data");
                        let mut trailers = http::HeaderMap::new();
                        trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
                        stream.send_trailers(trailers).expect("trailers");
                    });
                }
            });
        }
    });
    Ok((port, task))
}

fn request(
    port: u16,
    method: &str,
    path: &str,
    payment: Option<&str>,
    extra: &str,
    body: &[u8],
) -> anyhow::Result<(u16, String)> {
    let mut socket = TcpStream::connect(("127.0.0.1", port))?;
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    let payment = payment
        .map(|p| {
            if p.starts_with("L402 ") {
                format!("Authorization: {p}\r\n")
            } else {
                format!("PAYMENT-SIGNATURE: {p}\r\n")
            }
        })
        .unwrap_or_default();
    write!(
        socket,
        "{method} {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\nContent-Length: {}\r\n{payment}{extra}\r\n",
        body.len()
    )?;
    socket.write_all(body)?;
    let mut response = String::new();
    socket.read_to_string(&mut response)?;
    let status = response
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("no response: {response}"))?
        .parse()?;
    Ok((status, response))
}
fn header<'a>(response: &'a str, name: &str) -> Option<&'a str> {
    response
        .split("\r\n")
        .skip(1)
        .take_while(|l| !l.is_empty())
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then_some(value.trim())
        })
}
fn proof(challenge: &str, preimage: u8) -> anyhow::Result<String> {
    let required: Value = serde_json::from_slice(
        &STANDARD.decode(
            header(challenge, "payment-required")
                .ok_or_else(|| anyhow::anyhow!("missing challenge: {challenge}"))?,
        )?,
    )?;
    Ok(STANDARD.encode(json!({"x402Version": 2, "accepted": required["accepts"][0], "payload": {"preimage": hex(&[preimage; 32])}}).to_string()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ldk_receiver_and_cashu_coexist_with_durable_bound_admission() -> anyhow::Result<()> {
    admission(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l402_x402_and_cashu_share_single_use_invoices() -> anyhow::Result<()> {
    admission(true).await
}

fn selected_proof(challenge: &str, preimage: u8, l402: bool) -> anyhow::Result<String> {
    if !l402 {
        return proof(challenge, preimage);
    }
    let auth = header(challenge, "www-authenticate").expect("L402 challenge");
    let (macaroon, invoice) = auth
        .strip_prefix("L402 macaroon=\"")
        .expect("scheme")
        .split_once("\", invoice=\"")
        .expect("invoice");
    let required: Value = serde_json::from_slice(
        &STANDARD.decode(header(challenge, "payment-required").expect("x402 challenge"))?,
    )?;
    assert_eq!(
        required["accepts"][0]["extra"]["invoice"],
        invoice.trim_end_matches('"')
    );
    Ok(format!("L402 {macaroon}:{}", hex(&[preimage; 32])))
}

async fn admission(l402: bool) -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let (pem, cert, key) = certificate()?;
    std::fs::write(root.path().join("tls.crt"), pem)?;
    std::fs::write(root.path().join("api_key"), [99; 32])?;
    let count = Arc::new(AtomicUsize::new(0));
    let (receiver_port, receiver_task) = receiver(count.clone(), cert, key).await?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let tls_port = listener.local_addr()?.port();
    drop(listener);
    let backend = std::net::TcpListener::bind("127.0.0.1:0")?;
    let backend_port = backend.local_addr()?.port();
    let backend_thread = std::thread::spawn(move || -> anyhow::Result<()> {
        for index in 0..2 {
            let (mut socket, _) = backend.accept()?;
            socket.set_read_timeout(Some(Duration::from_secs(10)))?;
            let mut reader = BufReader::new(socket.try_clone()?);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                anyhow::ensure!(
                    reader.read_line(&mut line)? > 0,
                    "incomplete backend headers"
                );
                headers.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            assert!(header(&headers, "payment-signature").is_none());
            assert!(header(&headers, "x-cashu").is_none());
            if l402 {
                assert!(header(&headers, "authorization").is_none());
            }
            let length: usize = header(&headers, "content-length").unwrap_or("0").parse()?;
            let mut body = vec![0; length];
            reader.read_exact(&mut body)?;
            if index == 1 {
                assert_eq!(body, b"body", "paid body must reach the backend unchanged");
            }
            socket.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nadmitted",
            )?;
        }
        Ok(())
    });
    let config = root.path().join("Geatafile");
    std::fs::write(
        &config,
        format!(
            "lightning node {{ endpoint https://localhost:{receiver_port} api_key_file {} tls_cert_file {} pay_to 0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798 network mainnet }}\nhttp://localhost {{\nrate_limit 1/s burst 1\npay_over_limit 2 sat https://mint.example.com\nlightning_over_limit 25 sat node\nlightning_headers {}\n{}\nlightning_origin http://localhost:{port}\nreverse_proxy 127.0.0.1:{backend_port}\n}}",
            root.path().join("api_key").display(),
            root.path().join("tls.crt").display(),
            if l402 {
                "content-type"
            } else {
                "authorization content-type"
            },
            if l402 {
                "lightning_protocols x402 l402"
            } else {
                ""
            },
        ),
    )?;
    let start = || -> anyhow::Result<Process> {
        let child = Command::new(env!("CARGO_BIN_EXE_geata"))
            .arg("run")
            .arg("--config")
            .arg(&config)
            .arg("--data-dir")
            .arg(root.path().join("state"))
            .arg("--http-listen")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--https-listen")
            .arg(format!("127.0.0.1:{tls_port}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut process = Process(child);
        for _ in 0..200 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Ok(process);
            }
            anyhow::ensure!(
                process.0.try_wait()?.is_none(),
                "geata exited during startup"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        anyhow::bail!("geata startup timed out")
    };
    let process = start()?;
    assert_eq!(request(port, "GET", "/", None, "", b"")?.0, 200);
    let (status, challenge) = request(
        port,
        "POST",
        "/article?a=1",
        None,
        "Content-Type: text/plain\r\n",
        b"body",
    )?;
    assert_eq!(status, 402, "{challenge}");
    assert!(header(&challenge, "x-cashu").is_some());
    let paid = selected_proof(&challenge, 1, l402)?;
    let invalid_status = if l402 { 401 } else { 400 };
    assert_eq!(
        request(
            port,
            "POST",
            "/article?a=2",
            Some(&paid),
            "Content-Type: text/plain\r\n",
            b"body"
        )?
        .0,
        invalid_status
    );
    assert_eq!(
        request(
            port,
            "POST",
            "/article?a=1",
            Some(&paid),
            "Content-Type: text/plain\r\n",
            b"different"
        )?
        .0,
        invalid_status
    );
    assert_eq!(
        request(
            port,
            "POST",
            "/article?a=1",
            Some(&paid),
            "Content-Type: text/plain\r\nAuthorization: other\r\n",
            b"body"
        )?
        .0,
        invalid_status
    );
    assert_eq!(
        request(
            port,
            "POST",
            "/article?a=1",
            Some(&paid),
            "Content-Type: text/plain\r\nX-Cashu: cashuBbad\r\n",
            b"body"
        )?
        .0,
        400
    );
    let (status, response) = request(
        port,
        "POST",
        "/article?a=1",
        Some(&paid),
        "Content-Type: text/plain\r\n",
        b"body",
    )?;
    assert_eq!(status, 200, "{response}");
    assert_eq!(header(&response, "payment-response").is_some(), !l402);
    assert!(response.ends_with("admitted"));
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "paid retries must not issue more invoices"
    );
    assert_eq!(
        request(
            port,
            "POST",
            "/article?a=1",
            Some(&paid),
            "Content-Type: text/plain\r\n",
            b"body"
        )?
        .0,
        invalid_status
    );
    if l402 {
        let x402 = proof(&challenge, 1)?;
        assert_eq!(
            request(
                port,
                "POST",
                "/article?a=1",
                Some(&x402),
                "Content-Type: text/plain\r\n",
                b"body"
            )?
            .0,
            400
        );
    }
    drop(process);
    backend_thread.join().expect("backend thread")?;
    // Restart into a direct-response handler while preserving the replay database.
    let updated = std::fs::read_to_string(&config)?.replace(
        &format!("reverse_proxy 127.0.0.1:{backend_port}"),
        "respond admitted",
    );
    std::fs::write(&config, updated)?;
    let _process = start()?;
    assert_eq!(
        request(
            port,
            "POST",
            "/article?a=1",
            Some(&paid),
            "Content-Type: text/plain\r\n",
            b"body"
        )?
        .0,
        invalid_status
    );
    assert_eq!(request(port, "GET", "/", None, "", b"")?.0, 200);
    let (status, challenge) = request(port, "GET", "/", None, "", b"")?;
    assert_eq!(status, 402);
    let paid = selected_proof(&challenge, 2, l402)?;
    let (status, response) = request(port, "GET", "/", Some(&paid), "", b"")?;
    assert_eq!(status, 200);
    assert_eq!(header(&response, "payment-response").is_some(), !l402);
    if l402 {
        drop(_process);
        let updated = std::fs::read_to_string(&config)?
            .replace("lightning_protocols x402 l402", "lightning_protocols l402");
        std::fs::write(&config, updated)?;
        let process = start()?;
        assert_eq!(request(port, "GET", "/", None, "", b"")?.0, 200);
        let (status, challenge) = request(port, "GET", "/", None, "", b"")?;
        assert_eq!(status, 402);
        assert!(header(&challenge, "payment-required").is_none());
        let token = header(&challenge, "www-authenticate")
            .expect("L402 challenge")
            .strip_prefix("L402 macaroon=\"")
            .expect("scheme")
            .split('"')
            .next()
            .expect("macaroon");
        let paid = format!("L402 {token}:{}", hex(&[3; 32]));
        assert_eq!(
            request(port, "GET", "/", Some("disabled x402"), "", b"")?.0,
            400
        );
        // Outstanding credentials, including their root keys, survive restart.
        drop(process);
        let _restarted = start()?;
        assert_eq!(request(port, "GET", "/", Some(&paid), "", b"")?.0, 200);
        assert_eq!(request(port, "GET", "/", Some(&paid), "", b"")?.0, 401);
    }
    receiver_task.abort();
    Ok(())
}
