//! Process-wide transport admission, independent of site/request admission.
use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use async_trait::async_trait;
use pingora::{
    apps::{HttpServerApp, HttpServerOptions, ServerApp},
    listeners::PreTlsProcess,
    protocols::{ALPN, GetSocketDigest, Stream, l4::stream::Stream as L4Stream},
    upstreams::peer::{Tracer, Tracing},
};

use crate::{
    config::{Capacity, CapacityPermit},
    proxy::{BoundedProxy, Proxy},
};

pub(crate) const CAPACITY_ERROR: &str = "connection capacity exhausted";

#[derive(Debug)]
pub struct Connections(Capacity);

impl Default for Connections {
    fn default() -> Self {
        let mut capacity = Capacity::new(1024);
        capacity.per_client = 64;
        Self(capacity)
    }
}

impl Connections {
    fn acquire(&self, stream: &dyn GetSocketDigest) -> pingora::Result<CapacityPermit> {
        stream
            .get_socket_digest()
            .and_then(|d| d.peer_addr().and_then(|a| a.as_inet()).map(|a| a.ip()))
            .and_then(|ip| self.0.try_acquire(ip))
            .ok_or_else(|| {
                // There is no HTTP session to account for this rejection later.
                crate::proxy::record_rejection(0);
                pingora::Error::explain(pingora::ErrorType::ConnectError, CAPACITY_ERROR)
            })
    }
}

// The L4 stream owns this guard, including during failed/cancelled TLS handshakes.
#[derive(Clone, Debug)]
struct ConnectionGuard(Arc<CapacityPermit>);
impl Tracing for ConnectionGuard {
    fn on_connected(&self) {}
    fn on_disconnected(&self) {}
    fn boxed_clone(&self) -> Box<dyn Tracing> {
        Box::new(Self(self.0.clone()))
    }
}

#[async_trait]
impl PreTlsProcess for Connections {
    async fn process(&self, stream: &mut L4Stream) -> pingora::Result<()> {
        stream.tracer = Some(Tracer(Box::new(ConnectionGuard(Arc::new(
            self.acquire(stream)?,
        )))));
        Ok(())
    }
}

pub fn http_options() -> &'static HttpServerOptions {
    static OPTIONS: LazyLock<HttpServerOptions> = LazyLock::new(|| {
        let mut options = HttpServerOptions::default();
        options.h2_idle_timeout = Some(Duration::from_secs(30));
        options
    });
    &OPTIONS
}

pub struct ConnectionProxy {
    pub inner: Arc<pingora::proxy::HttpProxy<Proxy>>,
    pub connections: Arc<Connections>,
}

#[async_trait]
impl ServerApp for ConnectionProxy {
    async fn process_new(
        self: &Arc<Self>,
        stream: Stream,
        shutdown: &pingora::server::ShutdownWatch,
    ) -> Option<Stream> {
        // TLS streams already acquired capacity in PreTlsProcess. Plaintext
        // streams acquire it here, before Pingora reads any HTTP bytes.
        let _permit = if stream.get_ssl_digest().is_none() {
            match self.connections.acquire(&*stream) {
                Ok(permit) => Some(permit),
                Err(_) => return None,
            }
        } else {
            None
        };
        let is_h2 = matches!(stream.selected_alpn_proto(), Some(ALPN::H2));
        let (ready, receiver) = tokio::sync::watch::channel(false);
        let app = Arc::new(BoundedProxy {
            inner: self.inner.clone(),
            ready,
        });
        let connection = app.process_new(stream, shutdown);
        if is_h2 {
            // Bound negotiation AND the wait for the first request. This wraps
            // Pingora's handshake without replacing its validation, malformed
            // stream budget, idle tracking, or graceful shutdown machinery.
            return startup_deadline(connection, receiver).await.flatten();
        }
        connection.await
    }

    async fn cleanup(&self) {
        self.inner.http_cleanup().await;
    }
}

// Readiness is signalled by the first HTTP/2 request callback, after negotiation.
async fn startup_deadline<F: std::future::Future>(
    connection: F,
    mut receiver: tokio::sync::watch::Receiver<bool>,
) -> Option<F::Output> {
    tokio::pin!(connection);
    tokio::select! {
        result = &mut connection => return Some(result),
        _ = tokio::time::sleep(Duration::from_secs(10)) => return None,
        ready = receiver.wait_for(|ready| *ready) => {
            if ready.is_err() { return None; }
        }
    }
    Some(connection.await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_capacity_is_global_per_client_and_released_by_guards() {
        let connections = Connections::default();
        let mut guards = Vec::new();
        for client in 1..=16 {
            let ip = std::net::IpAddr::from([192, 0, 2, client]);
            for _ in 0..64 {
                guards.push(ConnectionGuard(Arc::new(
                    connections.0.try_acquire(ip).expect("slot"),
                )));
            }
            assert!(connections.0.try_acquire(ip).is_none());
        }
        let other = std::net::IpAddr::from([192, 0, 2, 17]);
        assert!(connections.0.try_acquire(other).is_none());
        let guard = guards.pop().expect("guard");
        let clone = guard.boxed_clone();
        drop(guard);
        assert!(connections.0.try_acquire(other).is_none());
        drop(clone);
        assert!(connections.0.try_acquire(other).is_some());
        drop(guards);
        assert!(connections.0.try_acquire([192, 0, 2, 1].into()).is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pre_tls_callback_holds_capacity_until_transport_drop() -> anyhow::Result<()> {
        use std::os::fd::AsRawFd;
        let wrap = |socket: tokio::net::TcpStream| {
            // Match the digest installed by Pingora's listener on acceptance.
            let digest = pingora::protocols::SocketDigest::from_raw_fd(socket.as_raw_fd());
            let mut stream = L4Stream::from(socket);
            stream.set_socket_digest(digest);
            stream
        };
        let connections = Connections(Capacity::new(1));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let _client = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
        let mut first = wrap(listener.accept().await?.0);
        connections.process(&mut first).await?;
        let _other = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
        let mut second = wrap(listener.accept().await?.0);
        assert!(connections.process(&mut second).await.is_err());
        // TLS failure/cancellation drops this same underlying transport.
        drop(first);
        connections.process(&mut second).await?;
        drop(second);
        assert!(connections.0.try_acquire([127, 0, 0, 1].into()).is_some());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn startup_timeout_cancels_connection_and_releases_capacity() {
        let connections = Connections::default();
        let ip = [192, 0, 2, 1].into();
        let mut guards = Vec::new();
        for _ in 0..63 {
            guards.push(connections.0.try_acquire(ip).expect("slot"));
        }
        let permit = connections.0.try_acquire(ip).expect("last slot");
        let (_sender, receiver) = tokio::sync::watch::channel(false);
        let connection = async move {
            let _permit = permit;
            std::future::pending::<()>().await;
        };
        assert!(startup_deadline(connection, receiver).await.is_none());
        assert!(connections.0.try_acquire(ip).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn startup_deadline_does_not_limit_active_connection_lifetime() {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let connection = async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            sender.send_replace(true);
            tokio::time::sleep(Duration::from_secs(60)).await;
            42
        };
        assert_eq!(startup_deadline(connection, receiver).await, Some(42));
    }
}
