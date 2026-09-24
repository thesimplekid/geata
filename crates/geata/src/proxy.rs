use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use pingora::{
    Error, ErrorType, Result,
    http::{RequestHeader, ResponseHeader},
    prelude::HttpPeer,
    proxy::{ProxyHttp, Session},
};

use crate::{
    config::{CapacityPermit, Handler, Site, request_domain},
    payments::PaymentError,
    state::State,
};

pub struct Proxy {
    pub state: Arc<State>,
    pub tls: bool,
    pub https_port: u16,
}

pub struct RequestContext {
    site: Option<Site>,
    host: String,
    started: Instant,
    addresses: Vec<SocketAddr>,
    address_index: usize,
    _capacity: Option<CapacityPermit>,
    lightning_receipt: Option<String>,
}

#[async_trait]
impl ProxyHttp for Proxy {
    type CTX = RequestContext;

    fn new_ctx(&self) -> Self::CTX {
        RequestContext {
            site: None,
            host: String::new(),
            started: Instant::now(),
            addresses: Vec::new(),
            address_index: 0,
            _capacity: None,
            lightning_receipt: None,
        }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let header = session.req_header();
        let authority = header
            .headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .or_else(|| header.uri.authority().map(|a| a.as_str()));
        let Some(domain) = authority.and_then(request_domain) else {
            return respond(session, 400, "A valid Host header is required.\n", None).await;
        };
        ctx.host = authority.unwrap_or_default().to_owned();
        let config = self.state.config.load();
        let Some(site) = config.sites.get(&domain).cloned() else {
            return respond(
                session,
                404,
                "No site is configured for this hostname.\n",
                None,
            )
            .await;
        };
        if !self.tls {
            if let Some(token) = session
                .req_header()
                .uri
                .path()
                .strip_prefix("/.well-known/acme-challenge/")
            {
                let response = self
                    .state
                    .challenges
                    .read()
                    .get(&(domain.clone(), token.to_owned()))
                    .cloned();
                return match response {
                    Some(response) if session.req_header().method == http::Method::GET => {
                        respond(session, 200, &response, None).await
                    }
                    _ => respond(session, 404, "No active certificate challenge.\n", None).await,
                };
            }
            if site.https {
                let port = if self.https_port == 443 {
                    String::new()
                } else {
                    format!(":{}", self.https_port)
                };
                let path = session
                    .req_header()
                    .uri
                    .path_and_query()
                    .map_or("/", |p| p.as_str());
                let location = format!("https://{domain}{port}{path}");
                return respond(session, 308, "Redirecting to HTTPS.\n", Some(&location)).await;
            }
        } else if !site.https {
            return respond(
                session,
                404,
                "This site is configured for HTTP only.\n",
                None,
            )
            .await;
        }
        let client = session
            .client_addr()
            .and_then(|address| address.as_inet())
            .map(|address| address.ip());
        if let Some(capacity) = &site.capacity {
            let client = client.ok_or_else(|| {
                Error::explain(ErrorType::InternalError, "request has no client IP")
            })?;
            match capacity.try_acquire(client) {
                Some(permit) => ctx._capacity = Some(permit),
                None => {
                    return respond_with_headers(
                        session,
                        503,
                        "Site is at capacity.\n",
                        &[("Retry-After", "1"), ("Cache-Control", "no-store")],
                    )
                    .await;
                }
            }
        }
        let l402_enabled = site.lightning.as_ref().is_some_and(|p| p.protocols.l402);
        let l402_header = if l402_enabled {
            let values = session.req_header().headers.get_all("authorization");
            match values.iter().next() {
                Some(value) => {
                    let credential = value
                        .to_str()
                        .ok()
                        .and_then(|value| value.split_once(' '))
                        .filter(|(scheme, credential)| {
                            scheme.eq_ignore_ascii_case("L402")
                                && !credential.is_empty()
                                && credential.len()
                                    <= crate::payments::lightning::MAX_PAYMENT_HEADER
                        });
                    if values.iter().count() != 1 || credential.is_none() {
                        return respond_with_headers(
                            session,
                            401,
                            "This site requires an L402 Authorization credential.\n",
                            &[("Cache-Control", "no-store"), ("WWW-Authenticate", "L402")],
                        )
                        .await;
                    }
                    Some(credential.expect("checked credential").1.to_owned())
                }
                None => None,
            }
        } else {
            None
        };
        if l402_header.is_some()
            && (session
                .req_header()
                .headers
                .contains_key("payment-signature")
                || session.req_header().headers.contains_key("x-cashu"))
        {
            return respond(session, 400, "Select exactly one payment method.\n", None).await;
        }
        if site.lightning.as_ref().is_some_and(|p| !p.protocols.x402)
            && session
                .req_header()
                .headers
                .contains_key("payment-signature")
        {
            return respond(session, 400, "Lightning x402 is disabled.\n", None).await;
        }
        let lightning_header = if site.lightning.is_some() {
            let values = session.req_header().headers.get_all("payment-signature");
            if values.iter().count() > 1
                || (values.iter().next().is_some()
                    && session.req_header().headers.contains_key("x-cashu"))
            {
                return respond(session, 400, "Select exactly one payment method.\n", None).await;
            }
            match values.iter().next() {
                Some(value) => match value.to_str() {
                    Ok(value) if value.len() <= crate::payments::lightning::MAX_PAYMENT_HEADER => {
                        Some(value.to_owned())
                    }
                    _ => {
                        return respond(session, 400, "Invalid Lightning payment header.\n", None)
                            .await;
                    }
                },
                None => None,
            }
        } else {
            None
        };
        let payment_headers = session.req_header().headers.get_all("X-Cashu");
        let supplied = site
            .payment
            .as_ref()
            .and_then(|_| payment_headers.iter().next());
        let using_l402 = l402_header.is_some();
        if let Some(encoded) = l402_header.or(lightning_header) {
            if !self.tls && !client.is_some_and(|ip| ip.is_loopback()) {
                return respond(session, 400, "Use HTTPS to send payments.\n", None).await;
            }
            let policy = site.lightning.as_ref().expect("Lightning policy checked");
            let Some(lightning) = &self.state.lightning else {
                return respond(session, 503, "Lightning unavailable.\n", None).await;
            };
            let binding = lightning_binding(session, policy, self.tls).await;
            let Ok((_, hash)) = binding else {
                return respond(
                    session,
                    400,
                    "Unsupported Lightning request binding or body size.\n",
                    None,
                )
                .await;
            };
            let settlement = if using_l402 {
                lightning
                    .settle_l402(policy, &hash, &encoded)
                    .await
                    .map(|()| None)
            } else {
                lightning.settle(policy, &hash, &encoded).await.map(Some)
            };
            match settlement {
                Ok(receipt) => ctx.lightning_receipt = receipt,
                Err(error) => {
                    let status = match error {
                        crate::payments::lightning::Error::Invalid => {
                            if using_l402 {
                                401
                            } else {
                                400
                            }
                        }
                        _ => 503,
                    };
                    let mut headers = vec![("Cache-Control", "no-store")];
                    if status == 401 {
                        headers.push(("WWW-Authenticate", "L402"));
                    }
                    return respond_with_headers(session, status, &format!("{error}\n"), &headers)
                        .await;
                }
            }
        } else if let Some(value) = supplied {
            if payment_headers.iter().count() != 1 {
                return respond(session, 400, "Provide exactly one X-Cashu token.\n", None).await;
            }
            let Some(policy) = &site.payment else {
                return respond(
                    session,
                    400,
                    "This site does not accept Cashu payments.\n",
                    None,
                )
                .await;
            };
            if !self.tls
                && !session
                    .client_addr()
                    .and_then(|a| a.as_inet())
                    .is_some_and(|a| a.ip().is_loopback())
            {
                return respond(session, 400, "Use HTTPS to send payment tokens.\n", None).await;
            }
            let Ok(encoded) = value.to_str() else {
                return respond(session, 400, "Invalid Cashu token.\n", None).await;
            };
            let Some(payments) = &self.state.payments else {
                return respond(session, 503, "Payments are unavailable.\n", None).await;
            };
            match payments.collect(&domain, policy, encoded).await {
                Ok(()) => {}
                Err(PaymentError::Invalid) => {
                    return respond_with_headers(
                        session,
                        400,
                        "Invalid, insufficient, or already used Cashu payment.\n",
                        &[("Cache-Control", "no-store")],
                    )
                    .await;
                }
                Err(PaymentError::Unavailable) => {
                    return respond_with_headers(
                        session,
                        503,
                        "Payments temporarily unavailable; retry the same token.\n",
                        &[("Retry-After", "1"), ("Cache-Control", "no-store")],
                    )
                    .await;
                }
            }
        } else if let Some(limiter) = &site.rate_limit {
            let client = client.ok_or_else(|| {
                Error::explain(ErrorType::InternalError, "request has no client IP")
            })?;
            if let Err(wait) = limiter.check(client) {
                let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() != 0);
                let retry_after = seconds.max(1).to_string();
                if site.payment.is_some() || site.lightning.is_some() {
                    let cashu = site.payment.as_ref().map(|p| p.challenge());
                    let mut required = None;
                    if let (Some(policy), Some(lightning)) =
                        (&site.lightning, &self.state.lightning)
                        && let Ok((url, hash)) = lightning_binding(session, policy, self.tls).await
                    {
                        required = lightning.challenge(policy, &url, &hash, client).await.ok();
                    }
                    if cashu.is_none() && required.is_none() {
                        return respond_with_headers(
                            session,
                            503,
                            "Lightning challenge unavailable.\n",
                            &[("Retry-After", "1"), ("Cache-Control", "no-store")],
                        )
                        .await;
                    }
                    let mut headers = vec![
                        ("Retry-After", retry_after.as_str()),
                        ("Cache-Control", "no-store"),
                    ];
                    if let Some(cashu) = &cashu {
                        headers.push(("X-Cashu", cashu));
                    }
                    if let Some(required) = &required {
                        if let Some(x402) = &required.x402 {
                            headers.push(("PAYMENT-REQUIRED", x402));
                        }
                        if let Some(l402) = &required.l402 {
                            headers.push(("WWW-Authenticate", l402));
                        }
                    }
                    return respond_with_headers(
                        session,
                        402,
                        "Payment required, or wait for the free allowance.\n",
                        &headers,
                    )
                    .await;
                }
                return respond_with_headers(
                    session,
                    429,
                    "Too many requests.\n",
                    &[("Retry-After", &retry_after), ("Cache-Control", "no-store")],
                )
                .await;
            }
        }
        if l402_enabled {
            session.req_header_mut().remove_header("authorization");
        }
        // Never forward bearer tokens, including on backend retries.
        if site.payment.is_some() || site.lightning.is_some() {
            session.req_header_mut().remove_header("X-Cashu");
            session.req_header_mut().remove_header("payment-signature");
        }
        if let Handler::Respond { body, status } = &site.handler {
            let mut headers = Vec::new();
            if site.payment.is_some() || site.lightning.is_some() {
                headers.push(("Cache-Control", "no-store"));
            }
            if let Some(receipt) = &ctx.lightning_receipt {
                headers.push(("PAYMENT-RESPONSE", receipt.as_str()));
            }
            return respond_with_headers(session, *status, body, &headers).await;
        }
        ctx.site = Some(site);
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let site = ctx
            .site
            .as_ref()
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "request has no route"))?;
        let Handler::Proxy(upstream) = &site.handler else {
            return Err(Error::explain(
                ErrorType::InternalError,
                "static response has no upstream",
            ));
        };
        if ctx.addresses.is_empty() {
            let addresses = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::lookup_host((upstream.host.as_str(), upstream.port)),
            )
            .await
            .map_err(|_| {
                Error::explain(ErrorType::ConnectTimedout, "backend DNS lookup timed out")
            })?
            .map_err(|_| Error::explain(ErrorType::ConnectError, "backend DNS lookup failed"))?;
            ctx.addresses = addresses.take(8).collect();
        }
        let address = ctx
            .addresses
            .get(ctx.address_index)
            .copied()
            .ok_or_else(|| Error::explain(ErrorType::ConnectError, "backend has no addresses"))?;
        let mut peer = HttpPeer::new(address, upstream.tls, upstream.host.clone());
        peer.options.connection_timeout = Some(Duration::from_secs(10));
        peer.options.total_connection_timeout = Some(Duration::from_secs(15));
        peer.options.read_timeout = Some(Duration::from_secs(300));
        peer.options.write_timeout = Some(Duration::from_secs(300));
        peer.options.idle_timeout = Some(Duration::from_secs(60));
        Ok(Box::new(peer))
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut error: Box<Error>,
    ) -> Box<Error> {
        // Retry another resolved address only before any request reaches a backend.
        // A retry after a reused connection closes must keep the current address.
        ctx.address_index += 1;
        error.set_retry(ctx.address_index < ctx.addresses.len());
        error
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if ctx
            .site
            .as_ref()
            .is_some_and(|site| site.payment.is_some() || site.lightning.is_some())
        {
            request.remove_header("X-Cashu");
            request.remove_header("payment-signature");
        }
        if ctx
            .site
            .as_ref()
            .and_then(|site| site.lightning.as_ref())
            .is_some_and(|p| p.protocols.l402)
        {
            request.remove_header("authorization");
        }
        // Direct internet clients cannot assert a trusted forwarding chain.
        for header in [
            "forwarded",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "x-forwarded-port",
            "x-real-ip",
        ] {
            request.remove_header(header);
        }
        if let Some(address) = session.client_addr().and_then(|a| a.as_inet()) {
            request.insert_header("X-Forwarded-For", address.ip().to_string())?;
            request.insert_header("X-Real-IP", address.ip().to_string())?;
        }
        request.insert_header("X-Forwarded-Proto", if self.tls { "https" } else { "http" })?;
        request.insert_header("X-Forwarded-Host", &ctx.host)?;
        if let Some(Site {
            handler: Handler::Proxy(upstream),
            ..
        }) = &ctx.site
        {
            request.insert_header(
                "Host",
                if upstream.tls {
                    &upstream.authority
                } else {
                    &ctx.host
                },
            )?;
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if ctx
            .site
            .as_ref()
            .is_some_and(|site| site.payment.is_some() || site.lightning.is_some())
        {
            response.insert_header("Cache-Control", "no-store")?;
            response.remove_header("payment-response");
            if let Some(receipt) = &ctx.lightning_receipt {
                response.insert_header("PAYMENT-RESPONSE", receipt)?;
            }
        }
        Ok(())
    }

    async fn logging(&self, session: &mut Session, error: Option<&Error>, ctx: &mut Self::CTX) {
        let status = session
            .response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(0);
        tracing::info!(host = %ctx.host, method = %session.req_header().method, path = %session.req_header().uri.path(), status, elapsed_ms = ctx.started.elapsed().as_millis() as u64, failed = error.is_some(), "request");
        if let Some(error) = error {
            tracing::warn!(%error, "proxy request failed");
        }
    }
}

async fn respond(
    session: &mut Session,
    status: u16,
    body: &str,
    location: Option<&str>,
) -> Result<bool> {
    let headers = location.map(|value| ("Location", value));
    respond_with_headers(session, status, body, headers.as_slice()).await
}

async fn respond_with_headers(
    session: &mut Session,
    status: u16,
    body: &str,
    headers: &[(&str, &str)],
) -> Result<bool> {
    let mut header = ResponseHeader::build(status, Some(3 + headers.len()))?;
    header.insert_header("Content-Type", "text/plain; charset=utf-8")?;
    let no_body = matches!(status, 204 | 205 | 304);
    // 204 must not carry Content-Length; 304 does not describe a selected representation here.
    if !matches!(status, 204 | 304) {
        header.insert_header("Content-Length", body.len().to_string())?;
    }
    for &(name, value) in headers {
        header.insert_header(name.to_owned(), value)?;
    }
    let end = session.req_header().method == http::Method::HEAD || no_body || body.is_empty();
    session.write_response_header(Box::new(header), end).await?;
    if !end {
        session
            .write_response_body(Some(Bytes::copy_from_slice(body.as_bytes())), true)
            .await?;
    }
    Ok(true)
}

// Buffer before settlement, so a body-changing retry cannot reuse an earlier proof.
// Pingora forwards its retry buffer to either HTTP/1 or HTTP/2 upstreams.
async fn lightning_binding(
    session: &mut Session,
    policy: &crate::payments::lightning::Policy,
    tls: bool,
) -> anyhow::Result<(String, String)> {
    use anyhow::ensure;
    let header = session.req_header();
    ensure!(
        !header.headers.contains_key("trailer") && !header.headers.contains_key("upgrade"),
        "trailers and upgrades are unsupported for Lightning"
    );
    let authority = header
        .headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .or_else(|| header.uri.authority().map(|a| a.as_str()))
        .ok_or_else(|| anyhow::anyhow!("missing host"))?;
    let origin = format!("{}://{authority}", if tls { "https" } else { "http" });
    ensure!(origin == policy.origin, "public origin mismatch");
    if let Some(scheme) = header.uri.scheme_str() {
        ensure!(
            scheme == if tls { "https" } else { "http" },
            "request scheme mismatch"
        );
    }
    if let Some(uri_authority) = header.uri.authority() {
        ensure!(
            uri_authority.as_str() == authority,
            "request authority mismatch"
        );
    }
    let path = header.uri.path_and_query().map_or("/", |p| p.as_str());
    ensure!(path.starts_with('/'), "unsupported request target");
    let url = format!("{origin}{path}");
    session.as_mut().enable_retry_buffering();
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut body = Vec::new();
        while let Some(chunk) = session.read_request_body().await? {
            ensure!(
                body.len() + chunk.len() <= crate::payments::lightning::MAX_BODY,
                "Lightning body exceeds 64 KiB"
            );
            body.extend_from_slice(&chunk);
        }
        ensure!(
            !session.as_ref().retry_buffer_truncated(),
            "request body could not be buffered"
        );
        let header = session.req_header();
        let hash = crate::payments::lightning::request_hash(
            policy,
            header.method.as_str(),
            &url,
            &header.headers,
            &body,
        )?;
        Ok((url, hash))
    })
    .await?
}
