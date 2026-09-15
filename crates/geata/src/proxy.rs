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
        let payment_headers = session.req_header().headers.get_all("X-Cashu");
        let supplied = site
            .payment
            .as_ref()
            .and_then(|_| payment_headers.iter().next());
        if let Some(value) = supplied {
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
                if let Some(payment) = &site.payment {
                    let challenge = payment.challenge();
                    return respond_with_headers(
                        session,
                        402,
                        "Payment required, or wait for the free allowance.\n",
                        &[
                            ("X-Cashu", &challenge),
                            ("Retry-After", &retry_after),
                            ("Cache-Control", "no-store"),
                        ],
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
        // Never forward bearer tokens, including on backend retries.
        if site.payment.is_some() {
            session.req_header_mut().remove_header("X-Cashu");
        }
        if let Handler::Respond { body, status } = &site.handler {
            let headers = if site.payment.is_some() {
                &[("Cache-Control", "no-store")][..]
            } else {
                &[]
            };
            return respond_with_headers(session, *status, body, headers).await;
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
        if ctx.site.as_ref().is_some_and(|site| site.payment.is_some()) {
            request.remove_header("X-Cashu");
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
        if ctx.site.as_ref().is_some_and(|site| site.payment.is_some()) {
            response.insert_header("Cache-Control", "no-store")?;
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
