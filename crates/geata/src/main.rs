mod acme_http;
mod automation;
mod certificates;
mod config;
mod proxy;
mod rate_limit;
mod retry;
mod state;
mod storage;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, ensure};
use clap::{Parser, Subcommand};
use pingora::{
    listeners::tls::TlsSettings,
    server::{Server, configuration::ServerConf},
    services::background::background_service,
};

use crate::{
    automation::Automation, certificates::DynamicTls, config::Config, proxy::Proxy, state::State,
    storage::Storage,
};

#[derive(Parser)]
#[command(
    name = "geata",
    version,
    about = "Simple reverse proxy with automatic HTTPS",
    after_help = "Public HTTPS requires DNS pointing to this server and inbound ports 80/443.\nRunning with public domains automatically registers with the CA and accepts its terms.\nUse --staging while testing certificate issuance."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve sites and automatically reload valid Geatafile edits.
    Run {
        #[arg(short, long, default_value = "Geatafile")]
        config: PathBuf,
        /// Private persistent directory for certificates and account credentials.
        #[arg(long, env = "PROXY_DATA_DIR")]
        data_dir: Option<PathBuf>,
        #[arg(long, default_value = "0.0.0.0:80")]
        http_listen: SocketAddr,
        #[arg(long, default_value = "0.0.0.0:443")]
        https_listen: SocketAddr,
        /// Use Let's Encrypt's test CA (certificates are not browser-trusted).
        #[arg(long, conflicts_with = "acme_directory")]
        staging: bool,
        /// Override the ACME directory, for example for a local test CA.
        #[arg(long, env = "PROXY_ACME_DIRECTORY")]
        acme_directory: Option<String>,
        /// Trust this PEM root for a private/test ACME server.
        #[arg(long, requires = "acme_directory")]
        acme_root: Option<PathBuf>,
        /// Optional certificate account contact email.
        #[arg(long, env = "PROXY_EMAIL")]
        email: Option<String>,
    },
    /// Check configuration without opening ports or contacting a CA.
    Validate {
        #[arg(short, long, default_value = "Geatafile")]
        config: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match Cli::parse().command {
        Command::Validate { config } => {
            let parsed = Config::read(&config)?;
            println!(
                "{} is valid ({} sites)",
                config.display(),
                parsed.sites.len()
            );
        }
        Command::Run {
            config,
            data_dir,
            http_listen,
            https_listen,
            staging,
            acme_directory,
            acme_root,
            email,
        } => {
            ensure!(
                http_listen.port() != 0 && https_listen.port() != 0,
                "listener ports must be nonzero"
            );
            ensure!(
                http_listen != https_listen,
                "HTTP and HTTPS listeners must use different addresses"
            );
            let initial_config = std::fs::read_to_string(&config)
                .with_context(|| format!("cannot read {}", config.display()))?;
            let parsed = Config::parse(&initial_config)?;
            // Preserve symlinks so atomic deployment swaps remain visible to reloads.
            let config_path = std::path::absolute(&config)?;
            let data_dir = data_dir.map(Ok).unwrap_or_else(default_data_dir)?;
            let directory_url = acme_directory.unwrap_or_else(|| {
                if staging {
                    instant_acme::LetsEncrypt::Staging
                } else {
                    instant_acme::LetsEncrypt::Production
                }
                .url()
                .to_owned()
            });
            let ca_url = url::Url::parse(&directory_url).context("invalid ACME directory URL")?;
            ensure!(
                ca_url.scheme() == "https" && ca_url.host_str().is_some(),
                "ACME directory must be an HTTPS URL"
            );
            let storage = Arc::new(Storage::open(&data_dir, &directory_url)?);
            let state = Arc::new(State::new(parsed));
            // Populate before accepting connections, so restart reuses certificates immediately.
            for domain in state.config.load().https_domains() {
                if let Some(cert) = storage.certificate(domain) {
                    state
                        .certificates
                        .write()
                        .insert(domain.to_owned(), Arc::new(cert));
                }
            }
            let conf = ServerConf {
                threads: std::thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(1)
                    .min(16),
                grace_period_seconds: Some(1),
                graceful_shutdown_timeout_seconds: Some(10),
                ..Default::default()
            };
            let mut server = Server::new_with_opt_and_conf(None, conf);
            server.bootstrap();
            let mut http = pingora::proxy::http_proxy_service(
                &server.configuration,
                Proxy {
                    state: state.clone(),
                    tls: false,
                    https_port: https_listen.port(),
                },
            );
            http.add_tcp(&http_listen.to_string());
            let mut https = pingora::proxy::http_proxy_service(
                &server.configuration,
                Proxy {
                    state: state.clone(),
                    tls: true,
                    https_port: https_listen.port(),
                },
            );
            let mut tls = TlsSettings::with_callbacks(Box::new(DynamicTls(state.clone())))?;
            tls.enable_h2();
            https.add_tls_with_settings(&https_listen.to_string(), None, tls);
            server.add_service(http);
            server.add_service(https);
            server.add_service(background_service(
                "certificates and configuration",
                Automation {
                    state,
                    storage,
                    config_path,
                    directory_url,
                    email,
                    acme_root,
                    initial_config,
                },
            ));
            tracing::info!(%http_listen, %https_listen, data_dir = %data_dir.display(), "starting proxy; watching Geatafile for changes");
            if staging {
                tracing::warn!("staging certificates are not browser-trusted");
            }
            server.run_forever();
        }
    }
    Ok(())
}

fn default_data_dir() -> anyhow::Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(
            std::env::var_os("HOME")
                .context("set --data-dir or PROXY_DATA_DIR when HOME is unavailable")?,
        )
        .join(".local/share"),
    };
    data_dir_under(&base)
}

fn data_dir_under(base: &std::path::Path) -> anyhow::Result<PathBuf> {
    let current = base.join("geata");
    let legacy = base.join("pingora-proxy");
    // Reuse existing certificates and the same lock without moving user data.
    if !current.try_exists()? && legacy.try_exists()? {
        return Ok(legacy);
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_storage_preserves_existing_certificates() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let current = root.path().join("geata");
        let legacy = root.path().join("pingora-proxy");
        assert_eq!(data_dir_under(root.path())?, current);
        std::fs::create_dir(&legacy)?;
        std::fs::write(legacy.join("saved-state"), b"existing credentials")?;
        assert_eq!(data_dir_under(root.path())?, legacy);
        assert_eq!(
            std::fs::read(legacy.join("saved-state"))?,
            b"existing credentials"
        );
        std::fs::create_dir(&current)?;
        assert_eq!(data_dir_under(root.path())?, current);
        Ok(())
    }
}
