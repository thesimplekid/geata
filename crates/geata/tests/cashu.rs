//! Local Cashu protocol fixture: real CDK signatures and swaps, no real money.
use cdk::{
    Amount, dhke,
    nuts::{
        BlindSignature, BlindedMessage, CurrencyUnit, Id, Keys, PaymentRequest, Proof, SecretKey,
        Token,
    },
    secret::Secret,
};
use redb::ReadableTable;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Mint {
    secrets: BTreeMap<Amount, SecretKey>,
    keys: Keys,
    id: Id,
    spent: Mutex<HashSet<String>>,
    signed: Mutex<HashMap<String, (Value, BlindSignature)>>,
    swaps: AtomicUsize,
    offline: AtomicBool,
    drop_swap_response: AtomicBool,
    slow: AtomicBool,
    backend: AtomicUsize,
    leaked_token: AtomicBool,
    payouts: Mutex<Vec<Value>>,
    fail_payout: AtomicBool,
}

impl Mint {
    fn new(v2: bool) -> Self {
        let secrets: BTreeMap<_, _> = (0..16)
            .map(|n| (Amount::from(1_u64 << n), SecretKey::generate()))
            .collect();
        let keys = Keys::new(
            secrets
                .iter()
                .map(|(amount, key)| (*amount, key.public_key()))
                .collect(),
        );
        let id = if v2 {
            Id::v2_from_data(&keys, &CurrencyUnit::Sat, 1000, None)
        } else {
            Id::v1_from_keys(&keys)
        };
        Self {
            secrets,
            keys,
            id,
            spent: Mutex::new(HashSet::new()),
            signed: Mutex::new(HashMap::new()),
            swaps: AtomicUsize::new(0),
            offline: AtomicBool::new(false),
            drop_swap_response: AtomicBool::new(false),
            slow: AtomicBool::new(false),
            backend: AtomicUsize::new(0),
            leaked_token: AtomicBool::new(false),
            payouts: Mutex::new(Vec::new()),
            fail_payout: AtomicBool::new(false),
        }
    }

    fn token(&self, url: &str, amount: u64) -> anyhow::Result<String> {
        let secret = Secret::generate();
        let (message, r) = dhke::blind_message(secret.as_bytes(), None)?;
        let key = &self.secrets[&Amount::from(amount)];
        let c = dhke::sign_message(key, &message)?;
        let signature = BlindSignature::new(Amount::from(amount), c, self.id, &message, key)?;
        let proofs = dhke::construct_proofs(vec![signature], vec![r], vec![secret], &self.keys)?;
        Ok(Token::new(url.parse()?, proofs, None, CurrencyUnit::Sat).to_string())
    }

    fn handle(
        &self,
        path: &str,
        headers: &HashMap<String, String>,
        body: &[u8],
    ) -> anyhow::Result<Option<(u16, Value)>> {
        if path == "/payout" {
            let payload: Value = serde_json::from_slice(body)?;
            let proofs: Vec<Proof> = serde_json::from_value(payload["proofs"].clone())?;
            for proof in &proofs {
                dhke::verify_message(
                    &self.secrets[&proof.amount],
                    proof.c,
                    proof.secret.as_bytes(),
                )?;
            }
            self.payouts.lock().expect("payout lock").push(payload);
            return Ok(Some((
                if self.fail_payout.load(Ordering::SeqCst) {
                    503
                } else {
                    200
                },
                json!({}),
            )));
        }
        if path.starts_with("/backend") {
            self.backend.fetch_add(1, Ordering::SeqCst);
            self.leaked_token
                .store(headers.contains_key("x-cashu"), Ordering::SeqCst);
            while self.slow.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(10));
            }
            return Ok(Some((200, json!({"ok": true}))));
        }
        if self.offline.load(Ordering::SeqCst) {
            return Ok(Some((503, json!({"error": "offline"}))));
        }
        let result = if path == "/v1/info" {
            json!({"name":"Geata test mint", "nuts": {
                "4":{"methods":[],"disabled":false}, "5":{"methods":[],"disabled":false},
                "7":{"supported":true},"8":{"supported":true},"9":{"supported":true},"12":{"supported":true}
            }})
        } else if path == "/v1/keysets" {
            json!({"keysets":[{"id":self.id,"unit":"sat","active":true,"input_fee_ppk":1000}]})
        } else if path.starts_with("/v1/keys") {
            json!({"keysets":[{"id":self.id,"unit":"sat","active":true,"input_fee_ppk":1000,"keys":self.keys}]})
        } else if path == "/v1/swap" {
            let data: Value = serde_json::from_slice(body)?;
            let inputs: Vec<Proof> = serde_json::from_value(data["inputs"].clone())?;
            let outputs: Vec<BlindedMessage> = serde_json::from_value(data["outputs"].clone())?;
            let mut spent = self.spent.lock().expect("mint lock");
            let ys: Vec<_> = inputs
                .iter()
                .map(|proof| dhke::hash_to_curve(proof.secret.as_bytes()).map(|y| y.to_string()))
                .collect::<Result<_, _>>()?;
            if ys.iter().any(|y| spent.contains(y)) {
                return Ok(Some((
                    400,
                    json!({"code":11001,"detail":"Token already spent"}),
                )));
            }
            for proof in &inputs {
                dhke::verify_message(
                    &self.secrets[&proof.amount],
                    proof.c,
                    proof.secret.as_bytes(),
                )?;
            }
            let input: u64 = inputs.iter().map(|p| u64::from(p.amount)).sum();
            let output: u64 = outputs.iter().map(|p| u64::from(p.amount)).sum();
            anyhow::ensure!(input == output + inputs.len() as u64, "incorrect swap fee");
            let mut signatures = vec![];
            for output in outputs {
                let c = dhke::sign_message(&self.secrets[&output.amount], &output.blinded_secret)?;
                let signature = BlindSignature::new(
                    output.amount,
                    c,
                    self.id,
                    &output.blinded_secret,
                    &self.secrets[&output.amount],
                )?;
                self.signed.lock().expect("signatures lock").insert(
                    output.blinded_secret.to_string(),
                    (serde_json::to_value(output)?, signature.clone()),
                );
                signatures.push(signature);
            }
            spent.extend(ys);
            self.swaps.fetch_add(1, Ordering::SeqCst);
            if self.drop_swap_response.swap(false, Ordering::SeqCst) {
                return Ok(None);
            }
            json!({"signatures":signatures})
        } else if path == "/v1/restore" {
            let data: Value = serde_json::from_slice(body)?;
            let outputs: Vec<BlindedMessage> = serde_json::from_value(data["outputs"].clone())?;
            let signed = self.signed.lock().expect("signatures lock");
            let matches: Vec<_> = outputs
                .iter()
                .filter_map(|output| signed.get(&output.blinded_secret.to_string()))
                .collect();
            json!({"outputs":matches.iter().map(|p| &p.0).collect::<Vec<_>>(), "signatures":matches.iter().map(|p| &p.1).collect::<Vec<_>>()})
        } else if path == "/v1/checkstate" {
            let data: Value = serde_json::from_slice(body)?;
            let ys: Vec<String> = serde_json::from_value(data["Ys"].clone())?;
            let spent = self.spent.lock().expect("mint lock");
            json!({"states":ys.iter().map(|y| json!({"Y": y, "state": if spent.contains(y) {"SPENT"} else {"UNSPENT"}})).collect::<Vec<_>>()})
        } else {
            return Ok(Some((404, json!({"error":path}))));
        };
        Ok(Some((200, result)))
    }
}

struct Server {
    port: u16,
    stopped: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn start(mint: Arc<Mint>) -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let stopped = Arc::new(AtomicBool::new(false));
        let quit = stopped.clone();
        let thread = thread::spawn(move || {
            while !quit.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let mint = mint.clone();
                        thread::spawn(move || {
                            if let Err(error) = serve(stream, &mint) {
                                eprintln!("test mint error: {error}");
                            }
                        });
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Ok(Self {
            port,
            stopped,
            thread: Some(thread),
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(mut stream: TcpStream, mint: &Mint) -> anyhow::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let path = first.split_whitespace().nth(1).unwrap_or("/");
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length: usize = headers
        .get("content-length")
        .map_or(Ok(0), |value| value.parse())?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    if let Some((status, value)) = mint.handle(path, &headers, &body)? {
        let body = value.to_string();
        write!(
            stream,
            "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
    }
    Ok(())
}

struct Proxy(Child, std::path::PathBuf);
impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        if thread::panicking() {
            eprintln!("{}", std::fs::read_to_string(&self.1).unwrap_or_default());
        }
    }
}
fn port() -> anyhow::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}
fn start(root: &Path, http: u16, https: u16) -> anyhow::Result<Proxy> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("proxy.log"))?;
    let child = Command::new(env!("CARGO_BIN_EXE_geata"))
        .args(["run", "--config"])
        .arg(root.join("Geatafile"))
        .arg("--data-dir")
        .arg(root.join("state"))
        .args([
            "--http-listen",
            &format!("127.0.0.1:{http}"),
            "--https-listen",
            &format!("127.0.0.1:{https}"),
        ])
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    let proxy = Proxy(child, root.join("proxy.log"));
    eventually(|| Ok(request(http, "ready.local", "/", None)?.0 == 200))?;
    Ok(proxy)
}
fn eventually(mut check: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if check().unwrap_or(false) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!("condition timed out")
}
fn request(
    port: u16,
    host: &str,
    path: &str,
    token: Option<&str>,
) -> anyhow::Result<(u16, HashMap<String, String>, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(40)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n"
    )?;
    if let Some(token) = token {
        write!(stream, "X-Cashu: {token}\r\n")?;
    }
    write!(stream, "\r\n")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("no response: {response}"))?;
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse()?;
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Ok((status, headers, body.to_owned()))
}

// EOF can reach the client just before Pingora drops the previous request's
// capacity permit. Only retry this non-charging transient in sequential tests;
// deliberate overload assertions continue to call request() directly.
fn settled_request(
    port: u16,
    host: &str,
    path: &str,
    token: Option<&str>,
) -> anyhow::Result<(u16, HashMap<String, String>, String)> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let response = request(port, host, path, token)?;
        if response.0 != 503 || response.2 != "Site is at capacity.\n" || Instant::now() >= deadline
        {
            return Ok(response);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn cashu_v1_redeems_once_and_recovers_without_double_charging() -> anyhow::Result<()> {
    cashu_overflow(false)
}

#[test]
fn cashu_v2_redeems_once_and_recovers_without_double_charging() -> anyhow::Result<()> {
    cashu_overflow(true)
}

fn cashu_overflow(v2: bool) -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let mint = Arc::new(Mint::new(v2));
    let server = Server::start(mint.clone())?;
    let mint_url = format!("http://127.0.0.1:{}", server.port);
    let (hp, tp) = (port()?, port()?);
    std::fs::write(
        root.path().join("Geatafile"),
        format!(
            r#"
http://ready.local {{ respond "ready" }}
http://localhost {{
    rate_limit 1/s burst 1
    pay_over_limit 2 sat {mint_url}
    max_inflight 1
    reverse_proxy 127.0.0.1:{}
}}
http://direct.local {{
    rate_limit 1/s burst 1
    pay_over_limit 2 sat {mint_url}
    respond "paid directly"
}}
"#,
            server.port
        ),
    )?;
    let mut proxy = Some(start(root.path(), hp, tp)?);
    let test = (|| -> anyhow::Result<()> {
        assert_eq!(settled_request(hp, "localhost", "/backend", None)?.0, 200);
        let (status, headers, _) = settled_request(hp, "localhost", "/backend", None)?;
        assert_eq!(status, 402);
        let payment = PaymentRequest::from_str(&headers["x-cashu"])?;
        assert_eq!(payment.amount, Some(Amount::from(2)));
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(
            settled_request(hp, "localhost", "/backend", Some("bad"))?.0,
            400
        );
        // Face value meets the advertised price, but cannot cover the input fee.
        let underpaid = mint.token(&mint_url, 2)?;
        assert_eq!(
            settled_request(hp, "localhost", "/backend", Some(&underpaid))?.0,
            400
        );
        assert_eq!(mint.swaps.load(Ordering::SeqCst), 0);
        let token = mint.token(&mint_url, 4)?;
        let paid = settled_request(hp, "localhost", "/backend", Some(&token))?;
        assert_eq!(paid.0, 200, "{}", paid.2);
        assert_eq!(paid.1["cache-control"], "no-store");
        assert_eq!(mint.swaps.load(Ordering::SeqCst), 1);
        assert!(!mint.leaked_token.load(Ordering::SeqCst));
        assert_eq!(
            settled_request(hp, "localhost", "/backend", Some(&token))?.0,
            400
        );
        assert_eq!(
            settled_request(hp, "direct.local", "/", Some(&token))?.0,
            400
        );
        let direct = mint.token(&mint_url, 4)?;
        assert_eq!(
            settled_request(hp, "direct.local", "/", Some(&direct))?.2,
            "paid directly"
        );
        // A public HTTP client cannot choose a mint via the token.
        let other = mint.token("http://127.0.0.1:1", 4)?;
        assert_eq!(
            settled_request(hp, "localhost", "/backend", Some(&other))?.0,
            400
        );
        let valid = mint.token(&mint_url, 4)?;
        let Token::TokenV4(mut forged) = Token::from_str(&valid)? else {
            anyhow::bail!("expected V4");
        };
        forged.token[0].proofs[0].dleq = None;
        assert_eq!(
            settled_request(
                hp,
                "localhost",
                "/backend",
                Some(&Token::TokenV4(forged).to_string())
            )?
            .0,
            400
        );
        assert_eq!(
            settled_request(hp, "localhost", "/backend", Some(&valid))?.0,
            200
        );
        let spent_elsewhere = mint.token(&mint_url, 4)?;
        for secret in Token::from_str(&spent_elsewhere)?.token_secrets() {
            mint.spent
                .lock()
                .expect("mint lock")
                .insert(dhke::hash_to_curve(secret.as_bytes())?.to_string());
        }
        assert_eq!(
            settled_request(hp, "localhost", "/backend", Some(&spent_elsewhere))?.0,
            400
        );
        let fresh = mint.token(&mint_url, 4)?;
        assert_eq!(
            settled_request(hp, "localhost", "/backend", Some(&fresh))?.0,
            200
        );
        let offline = mint.token(&mint_url, 4)?;
        mint.offline.store(true, Ordering::SeqCst);
        assert_eq!(
            settled_request(hp, "localhost", "/backend", Some(&offline))?.0,
            503
        );
        mint.offline.store(false, Ordering::SeqCst);
        assert_eq!(
            settled_request(hp, "localhost", "/backend", Some(&offline))?.0,
            200
        );
        // Capacity is reserved before redemption, including for paid requests.
        let slow_token = mint.token(&mint_url, 4)?;
        mint.slow.store(true, Ordering::SeqCst);
        let before = mint.backend.load(Ordering::SeqCst);
        let slow =
            thread::spawn(move || settled_request(hp, "localhost", "/backend", Some(&slow_token)));
        eventually(|| Ok(mint.backend.load(Ordering::SeqCst) > before))?;
        let waiting = mint.token(&mint_url, 4)?;
        let swaps = mint.swaps.load(Ordering::SeqCst);
        assert_eq!(request(hp, "localhost", "/backend", Some(&waiting))?.0, 503);
        assert_eq!(mint.swaps.load(Ordering::SeqCst), swaps);
        mint.slow.store(false, Ordering::SeqCst);
        assert_eq!(slow.join().expect("request thread")?.0, 200);
        assert_eq!(
            settled_request(hp, "localhost", "/backend", Some(&waiting))?.0,
            200
        );
        drop(proxy.take());
        proxy = Some(start(root.path(), hp, tp)?);
        assert_eq!(
            settled_request(hp, "localhost", "/backend", Some(&token))?.0,
            400
        );
        // Mint accepted the swap but its response was lost. Retry the same token
        // after restart: CDK restores outputs; Geata grants admission only once.
        let interrupted = mint.token(&mint_url, 4)?;
        let swaps = mint.swaps.load(Ordering::SeqCst);
        mint.drop_swap_response.store(true, Ordering::SeqCst);
        let first = settled_request(hp, "localhost", "/backend", Some(&interrupted))?;
        assert_eq!(first.0, 503, "{}", first.2);
        drop(proxy.take());
        proxy = Some(start(root.path(), hp, tp)?);
        let retry = settled_request(hp, "localhost", "/backend", Some(&interrupted))?;
        assert_eq!(retry.0, 200, "{}", retry.2);
        assert_eq!(mint.swaps.load(Ordering::SeqCst), swaps + 1);
        assert_eq!(
            settled_request(hp, "localhost", "/backend", Some(&interrupted))?.0,
            400
        );
        let wallet = |action: &[&str]| {
            Command::new(env!("CARGO_BIN_EXE_geata"))
                .args(["wallet", "--mint", &mint_url, "--data-dir"])
                .arg(root.path().join("state"))
                .args(action)
                .output()
        };
        assert!(
            !wallet(&["balance"])?.status.success(),
            "wallet command bypassed the running proxy's lock"
        );
        drop(proxy.take());
        let balance = wallet(&["balance"])?;
        assert!(
            balance.status.success(),
            "{}",
            String::from_utf8_lossy(&balance.stderr)
        );
        assert!(
            String::from_utf8_lossy(&balance.stdout)
                .contains(&format!("{} sat", mint.swaps.load(Ordering::SeqCst) * 3))
        );
        let exported = wallet(&["export", "--amount", "2"])?;
        assert!(
            exported.status.success(),
            "{}",
            String::from_utf8_lossy(&exported.stderr)
        );
        let output = String::from_utf8(exported.stdout)?;
        let token = output
            .lines()
            .find(|line| line.starts_with("cashuB"))
            .ok_or_else(|| anyhow::anyhow!("export did not return a token"))?;
        assert!(Token::from_str(token)?.value()? >= Amount::from(2));
        Ok(())
    })();
    mint.slow.store(false, Ordering::SeqCst);
    if test.is_err() || thread::panicking() {
        eprintln!(
            "{}",
            std::fs::read_to_string(root.path().join("proxy.log")).unwrap_or_default()
        );
    }
    drop(proxy);
    test
}

#[test]
fn payouts_deliver_enforce_fees_and_pause_uncertain_sends() -> anyhow::Result<()> {
    use cdk::nuts::{Transport, TransportType};
    let root = tempfile::tempdir()?;
    let mint = Arc::new(Mint::new(true));
    let server = Server::start(mint.clone())?;
    let mint_url = format!("http://127.0.0.1:{}", server.port);
    let payment = PaymentRequest::builder()
        .amount(16)
        .unit(CurrencyUnit::Sat)
        .payment_id("operator-payout")
        .single_use(false)
        .add_mint(mint_url.parse()?)
        .add_transport(Transport {
            _type: TransportType::HttpPost,
            target: format!("{mint_url}/payout"),
            tags: vec![],
        })
        .build()
        .to_string();
    let base = format!(
        "http://ready.local {{ respond ready }}\nhttp://localhost {{ respond paid rate_limit 1/s burst 1 pay_over_limit 2 sat {mint_url} }}\n"
    );
    let policy =
        format!("cashu_payout {{ mint {mint_url} request {payment} max_fee 10 threshold 50 }}\n");
    let config = root.path().join("Geatafile");
    std::fs::write(&config, format!("{base}{policy}"))?;
    let (hp, tp) = (port()?, port()?);
    let proxy = start(root.path(), hp, tp)?;
    let funding = mint.token(&mint_url, 64)?;
    eventually(|| Ok(request(hp, "localhost", "/", Some(&funding))?.0 == 200))?;
    eventually(|| {
        Ok(std::fs::read_to_string(root.path().join("proxy.log"))?
            .contains("automatic Cashu payout delivered"))
    })?;
    assert_eq!(request(hp, "ready.local", "/", None)?.0, 200);
    drop(proxy);
    let payload = mint.payouts.lock().expect("payout lock")[0].clone();
    assert_eq!(payload["id"], "operator-payout");
    assert_eq!(payload["unit"], "sat");
    assert_eq!(payload["mint"], mint_url);
    let proofs: Vec<Proof> = serde_json::from_value(payload["proofs"].clone())?;
    let net: u64 = proofs.iter().map(|p| u64::from(p.amount)).sum::<u64>() - proofs.len() as u64;
    assert!(net >= 16, "receiver fees were not covered");
    let wallet = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_geata"))
            .env("RUST_LOG", "off")
            .args(["wallet", "--mint", &mint_url, "--data-dir"])
            .arg(root.path().join("state"))
            .args(args)
            .output()
    };
    let balance = || -> anyhow::Result<u64> {
        let output = wallet(&["balance"])?;
        anyhow::ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?
            .split_whitespace()
            .next()
            .expect("balance")
            .parse()?)
    };
    let before = balance()?;
    assert!(before < 50);
    expire_payout_timer(root.path())?;
    // A restart does not repeat a threshold payout after balance falls below it.
    let proxy = start(root.path(), hp, tp)?;
    thread::sleep(Duration::from_secs(6));
    assert_eq!(mint.payouts.lock().expect("payout lock").len(), 1);
    drop(proxy);
    let capped = wallet(&["pay", &payment, "--max-total", "16"])?;
    assert!(!capped.status.success());
    assert_eq!(balance()?, before, "fee rejection lost reserved funds");
    assert_eq!(mint.payouts.lock().expect("payout lock").len(), 1);
    mint.fail_payout.store(true, Ordering::SeqCst);
    let failed = wallet(&["pay", &payment, "--max-total", "26"])?;
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("outcome uncertain"),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    assert_eq!(mint.payouts.lock().expect("payout lock").len(), 2);
    let pending = wallet(&["pending"])?;
    assert!(
        pending.status.success(),
        "{}",
        String::from_utf8_lossy(&pending.stderr)
    );
    let text = String::from_utf8(pending.stdout)?;
    let operation = text
        .lines()
        .find(|line| line.contains("payout unresolved"))
        .expect("recovery guard")
        .split_whitespace()
        .next()
        .expect("operation");
    expire_payout_timer(root.path())?;
    // Even a newly configured schedule must not bypass a previous uncertain send.
    std::fs::write(
        &config,
        format!("{base}{}", policy.replace("threshold 50", "interval 1s")),
    )?;
    let proxy = start(root.path(), hp, tp)?;
    thread::sleep(Duration::from_secs(6));
    assert_eq!(mint.payouts.lock().expect("payout lock").len(), 2);
    drop(proxy);
    let reclaimed = wallet(&["reclaim", "--operation", operation])?;
    assert!(
        reclaimed.status.success(),
        "{}",
        String::from_utf8_lossy(&reclaimed.stderr)
    );
    assert!(!String::from_utf8_lossy(&reclaimed.stdout).contains("payout unresolved"));
    assert!(balance()? > 16);
    // A receiver can redeem despite reporting delivery failure. Recovery must
    // recognize completion without reclaiming or issuing a replacement payment.
    assert!(
        !wallet(&["pay", &payment, "--max-total", "26"])?
            .status
            .success()
    );
    let delivered = mint
        .payouts
        .lock()
        .expect("payout lock")
        .last()
        .expect("payment")
        .clone();
    let proofs: Vec<Proof> = serde_json::from_value(delivered["proofs"].clone())?;
    for proof in proofs {
        mint.spent
            .lock()
            .expect("mint lock")
            .insert(dhke::hash_to_curve(proof.secret.as_bytes())?.to_string());
    }
    let completed = wallet(&["pending"])?;
    assert!(completed.status.success());
    assert!(!String::from_utf8_lossy(&completed.stdout).contains("payout unresolved"));
    Ok(())
}

#[test]
fn scheduled_nostr_payout_delivers_an_encrypted_payment() -> anyhow::Result<()> {
    use cdk::nuts::{Transport, TransportType};
    use nostr_sdk::{
        Event, ToBech32,
        nips::{nip19::Nip19Profile, nip59::UnwrappedGift},
    };
    let root = tempfile::tempdir()?;
    let mint = Arc::new(Mint::new(true));
    let server = Server::start(mint.clone())?;
    let mint_url = format!("http://127.0.0.1:{}", server.port);
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let relay = format!("ws://127.0.0.1:{}", listener.local_addr()?.port());
    let keys = nostr_sdk::Keys::generate();
    let profile = Nip19Profile::new(keys.public_key(), [relay.parse()?]);
    let payment = PaymentRequest::builder()
        .amount(8)
        .unit(CurrencyUnit::Sat)
        .single_use(false)
        .add_mint(mint_url.parse()?)
        .add_transport(Transport {
            _type: TransportType::Nostr,
            target: profile.to_bech32()?,
            tags: vec![vec!["n".into(), "17".into()]],
        })
        .build()
        .to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    let relay_thread = thread::spawn(move || -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let stream = loop {
            if let Ok((stream, _)) = listener.accept() {
                break stream;
            }
            anyhow::ensure!(Instant::now() < deadline, "relay connection timed out");
            thread::sleep(Duration::from_millis(20));
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut socket = tungstenite::accept(stream)?;
        loop {
            let message = socket.read()?;
            if let tungstenite::Message::Text(text) = message {
                let data: Value = serde_json::from_str(text.as_str())?;
                if data[0] == "EVENT" {
                    let event: Event = serde_json::from_value(data[1].clone())?;
                    event.verify()?;
                    assert_eq!(event.kind, nostr_sdk::Kind::GiftWrap);
                    assert!(!event.content.contains("proofs"));
                    socket.send(tungstenite::Message::Text(
                        json!(["OK", event.id, true, ""]).to_string().into(),
                    ))?;
                    tx.send(event)?;
                    break;
                }
            }
        }
        Ok(())
    });
    std::fs::write(
        root.path().join("Geatafile"),
        format!(
            "http://ready.local {{ respond ready }}\nhttp://localhost {{ respond paid rate_limit 1/s burst 1 pay_over_limit 2 sat {mint_url} }}\ncashu_payout {{ mint {mint_url} request {payment} interval 2s max_fee 10 }}"
        ),
    )?;
    let (hp, tp) = (port()?, port()?);
    let proxy = start(root.path(), hp, tp)?;
    let funding = mint.token(&mint_url, 32)?;
    eventually(|| Ok(request(hp, "localhost", "/", Some(&funding))?.0 == 200))?;
    let event = rx.recv_timeout(Duration::from_secs(20))?;
    let gift =
        tokio::runtime::Runtime::new()?.block_on(UnwrappedGift::from_gift_wrap(&keys, &event))?;
    let payload: Value = serde_json::from_str(&gift.rumor.content)?;
    assert_eq!(payload["mint"], mint_url);
    let proofs: Vec<Proof> = serde_json::from_value(payload["proofs"].clone())?;
    for proof in &proofs {
        dhke::verify_message(
            &mint.secrets[&proof.amount],
            proof.c,
            proof.secret.as_bytes(),
        )?;
    }
    assert!(proofs.iter().map(|p| u64::from(p.amount)).sum::<u64>() >= 8 + proofs.len() as u64);
    eventually(|| {
        Ok(std::fs::read_to_string(root.path().join("proxy.log"))?
            .contains("automatic Cashu payout delivered"))
    })?;
    assert_eq!(request(hp, "ready.local", "/", None)?.0, 200);
    drop(proxy);
    relay_thread.join().expect("relay thread")?;
    Ok(())
}

// Advance the durable test clock while the proxy is stopped, avoiding a minute
// of wall time. Preserve the operation guard so restart exercises uncertainty.
fn expire_payout_timer(root: &Path) -> anyhow::Result<()> {
    let mint = std::fs::read_dir(root.join("state/payments"))?
        .next()
        .expect("mint directory")?
        .path();
    let database = redb::Database::create(mint.join("ledger.redb"))?;
    let tx = database.begin_write()?;
    {
        let mut table = tx.open_table(redb::TableDefinition::<&str, &str>::new("payout_v1"))?;
        let mut state: Value =
            serde_json::from_str(table.get("state")?.expect("payout state").value())?;
        state["retry_at"] = json!(0);
        state["last_paid"] = json!(0);
        table.insert("state", serde_json::to_string(&state)?.as_str())?;
    }
    tx.commit()?;
    Ok(())
}
