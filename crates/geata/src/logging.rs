//! Suppress counted capacity rejections and sample client TLS failures.
//! Keep accept failures and internal listener diagnostics visible.
use tracing::{
    Event, Metadata, Subscriber,
    field::{Field, Visit},
};
use tracing_log::NormalizeEvent;
use tracing_subscriber::layer::{Context, Filter};

#[derive(Default)]
pub struct ListenerErrorFilter {
    handshake_sample: parking_lot::Mutex<HandshakeSample>,
}

#[derive(Default)]
struct HandshakeSample(Option<std::time::Instant>);
impl HandshakeSample {
    fn allow(&mut self, now: std::time::Instant) -> bool {
        if self
            .0
            .is_some_and(|last| now.duration_since(last) < std::time::Duration::from_secs(10))
        {
            return false;
        }
        self.0 = Some(now);
        true
    }
}

impl<S: Subscriber> Filter<S> for ListenerErrorFilter {
    fn enabled(&self, _: &Metadata<'_>, _: &Context<'_, S>) -> bool {
        true
    }

    fn event_enabled(&self, event: &Event<'_>, _: &Context<'_, S>) -> bool {
        // Pingora uses `log`; its original target is carried in event fields.
        let normalized = event.normalized_metadata();
        let metadata = normalized.as_ref().unwrap_or_else(|| event.metadata());
        if metadata.target() != "pingora_core::services::listening"
            || *metadata.level() != tracing::Level::ERROR
        {
            return true;
        }
        let mut message = Message::default();
        event.record(&mut message);
        if message.0.starts_with("Downstream handshake error")
            && message.0.ends_with(crate::connections::CAPACITY_ERROR)
        {
            return false;
        }
        let client_failure = message.0 == "Downstream handshake timeout"
            || (message.0.starts_with("Downstream handshake error")
                && message.0.contains("TLSHandshakeFailure")
                && message.0.contains("TLS accept() failed:"));
        !client_failure
            || self
                .handshake_sample
                .lock()
                .allow(std::time::Instant::now())
    }
}

#[derive(Default)]
struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };
    use tracing_subscriber::{Layer, filter::FilterExt, layer::SubscriberExt};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("capture lock")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn handshake_sample_is_shared_and_resumes_after_interval() {
        let mut sample = HandshakeSample::default();
        let now = std::time::Instant::now();
        assert!(sample.allow(now));
        for _ in 0..100 {
            assert!(!sample.allow(now));
        }
        assert!(!sample.allow(now + std::time::Duration::from_secs(9)));
        assert!(sample.allow(now + std::time::Duration::from_secs(10)));
    }

    #[test]
    fn suppresses_bridged_capacity_errors_but_preserves_other_diagnostics() {
        let output = Capture::default();
        let writer = output.clone();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .with_filter(
                    tracing_subscriber::EnvFilter::new("info").and(ListenerErrorFilter::default()),
                ),
        );
        tracing::subscriber::with_default(subscriber, || {
            let error = pingora::Error::explain(
                pingora::ErrorType::ConnectError,
                crate::connections::CAPACITY_ERROR,
            );
            for _ in 0..20 {
                // Match both Pingora listener callsites, through the actual
                // log-to-tracing bridge (whose raw event target is `log`).
                for prefix in [
                    "Downstream handshake error from 192.0.2.1:1234",
                    "Downstream handshake error",
                ] {
                    tracing_log::format_trace(
                        &log::Record::builder()
                            .target("pingora_core::services::listening")
                            .level(log::Level::Error)
                            .args(format_args!("{prefix}: {error}"))
                            .build(),
                    )
                    .expect("log bridge");
                }
            }
            for message in [
                "Downstream handshake error: InternalError context: listener configuration",
                "Downstream handshake error: TLSHandshakeFailure context: ssl_acceptor error",
                "Downstream handshake timeout",
                "Accept() failed",
            ] {
                tracing_log::format_trace(
                    &log::Record::builder()
                        .target("pingora_core::services::listening")
                        .level(log::Level::Error)
                        .args(format_args!("{message}"))
                        .build(),
                )
                .expect("log bridge");
            }
            for _ in 0..20 {
                let error = pingora::Error::explain(
                    pingora::ErrorType::TLSHandshakeFailure,
                    "TLS accept() failed: malformed client handshake",
                );
                tracing_log::format_trace(
                    &log::Record::builder()
                        .target("pingora_core::services::listening")
                        .level(log::Level::Error)
                        .args(format_args!(
                            "Downstream handshake error from 192.0.2.2:2345: {error}"
                        ))
                        .build(),
                )
                .expect("log bridge");
            }
            tracing::error!(target: "other_component", "Downstream handshake error: {}", crate::connections::CAPACITY_ERROR);
            tracing::info!(counts = ?[(0, 40)], "rejection summary");
        });
        let text = String::from_utf8(output.0.lock().expect("capture lock").clone()).expect("utf8");
        assert!(!text.contains("192.0.2.1"));
        assert!(
            !text.contains("malformed client handshake"),
            "timeout already spent the shared sample"
        );
        assert_eq!(text.matches(crate::connections::CAPACITY_ERROR).count(), 1);
        assert!(text.contains("other_component"));
        assert!(text.contains("listener configuration"));
        assert!(text.contains("ssl_acceptor error"));
        assert!(text.contains("handshake timeout"));
        assert!(text.contains("Accept() failed"));
        assert!(text.contains("rejection summary"));
    }
}
