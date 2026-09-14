use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail, ensure};
use async_trait::async_trait;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use pingora::{server::ShutdownWatch, services::background::BackgroundService};

use crate::{
    acme_http::{AcmeHttp, Cooldown},
    certificates::{Certificate, CertificatePem, now},
    config::Config,
    retry::Retries,
    state::State,
    storage::Storage,
};

pub struct Automation {
    pub state: Arc<State>,
    pub storage: Arc<Storage>,
    pub config_path: PathBuf,
    pub directory_url: String,
    pub email: Option<String>,
    pub acme_root: Option<PathBuf>,
    pub initial_config: String,
}

#[async_trait]
impl BackgroundService for Automation {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        tokio::select! {
            _ = shutdown.changed() => {},
            _ = self.watch_config() => {},
            _ = self.manage_certificates() => {},
        }
    }
}

impl Automation {
    async fn watch_config(&self) {
        let mut previous = self.initial_config.clone();
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            match std::fs::read_to_string(&self.config_path) {
                Ok(text) if text != previous => {
                    previous.clone_from(&text);
                    match Config::parse(&text) {
                        Ok(config) => {
                            let count = config.sites.len();
                            self.state.replace_config(config);
                            tracing::info!(sites = count, "configuration reloaded");
                        }
                        Err(error) => {
                            tracing::error!(%error, "configuration rejected; keeping previous sites")
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "cannot read Geatafile; keeping previous sites")
                }
            }
        }
    }

    async fn account(
        &self,
        cooldown: Arc<Cooldown>,
    ) -> anyhow::Result<(Account, Option<AccountCredentials>)> {
        let builder = Account::builder_with_http(Box::new(AcmeHttp::new(
            self.acme_root.as_deref(),
            cooldown,
        )?));
        if let Some(credentials) = self.storage.read("account.json")? {
            return Ok((builder.from_credentials(credentials).await?, None));
        }
        let contacts: Vec<String> = self
            .email
            .iter()
            .map(|email| format!("mailto:{email}"))
            .collect();
        let contacts: Vec<&str> = contacts.iter().map(String::as_str).collect();
        let (account, credentials) = builder
            .create(
                &NewAccount {
                    contact: &contacts,
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                self.directory_url.clone(),
                None,
            )
            .await?;
        Ok((account, Some(credentials)))
    }

    async fn manage_certificates(&self) {
        loop {
            if let Err(error) = self.certificate_loop().await {
                // A damaged retry journal must not silently reset a CA-imposed wait.
                tracing::error!(%error, "cannot load certificate retry state; existing sites remain available");
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    async fn certificate_loop(&self) -> anyhow::Result<()> {
        let cooldown = Arc::new(Cooldown::load(self.storage.clone())?);
        let mut retries: Retries = self.storage.read("retries.json")?.unwrap_or_default();
        let mut account = None;
        let mut credentials = None;
        let mut loaded = HashSet::new();
        let mut workers = Workers::default();
        let mut pending: HashMap<String, PendingCertificate> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut save_at = 0;
        loop {
            tokio::select! {
                _ = tick.tick() => {},
                result = workers.jobs.join_next_with_id(), if !workers.jobs.is_empty() => {
                    if let Some(result) = result {
                        match result {
                            Ok((id, result)) => {
                                if let Some(domain) = workers.remove(id) {
                                    match result {
                                        Ok(cert) => { pending.insert(domain, cert); }
                                        Err(error) => {
                                            retries.domains.entry(domain.clone()).or_default().failed(now(), self.expiry(&domain));
                                            tracing::error!(%domain, %error, "certificate request failed; will retry automatically");
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                if let Some(domain) = workers.remove(error.id()) {
                                    retries.domains.entry(domain.clone()).or_default().failed(now(), self.expiry(&domain));
                                    tracing::error!(%domain, %error, "certificate worker stopped; will retry automatically");
                                }
                            }
                        }
                        if let Err(error) = self.storage.write("retries.json", &retries) {
                            tracing::error!(%error, "cannot save retry state");
                        }
                    }
                }
            }
            let domains: HashSet<String> = self
                .state
                .config
                .load()
                .https_domains()
                .map(str::to_owned)
                .collect();
            workers.retain(&domains);
            loaded.retain(|domain| domains.contains(domain));
            pending.retain(|domain, _| domains.contains(domain));
            self.state
                .certificates
                .write()
                .retain(|domain, _| domains.contains(domain));
            for domain in &domains {
                if loaded.insert(domain.clone())
                    && !self.state.certificates.read().contains_key(domain)
                    && let Some(cert) = self.storage.certificate(domain)
                {
                    self.state
                        .certificates
                        .write()
                        .insert(domain.clone(), Arc::new(cert));
                }
            }

            // Keep issued material in memory while the disk is unavailable. Do not
            // obtain another certificate or register another account just to retry a save.
            if now() >= save_at {
                save_at = now() + 5;
                let mut saved_certificate = false;
                if let Some(value) = &credentials {
                    match self.storage.write("account.json", value) {
                        Ok(()) => {
                            credentials = None;
                        }
                        Err(error) => {
                            tracing::error!(%error, "cannot save account; retaining credentials and pausing issuance")
                        }
                    }
                }
                pending.retain(|domain, cert| {
                    if !cert.certificate.valid_at(now()) {
                        return false;
                    }
                    match cert.install(domain, &self.storage, &self.state) {
                        Ok(()) => {
                            retries.domains.remove(domain);
                            saved_certificate = true;
                            tracing::info!(%domain, "HTTPS ready; certificate saved and renewal scheduled");
                            false
                        }
                        Err(error) => {
                            tracing::error!(%domain, %error, "cannot save certificate; retaining it for another save attempt");
                            true
                        }
                    }
                });
                if saved_certificate
                    && let Err(error) = self.storage.write("retries.json", &retries)
                {
                    tracing::error!(%error, "cannot clear completed certificate retries");
                }
            }
            if credentials.is_some() || !cooldown.ready() || workers.jobs.len() >= 4 {
                continue;
            }
            if account.is_none() && !retries.account.ready(now()) {
                continue;
            }
            let mut needed: Vec<_> = domains
                .iter()
                .filter(|domain| {
                    !workers.active.contains_key(*domain)
                        && !pending.contains_key(*domain)
                        && self
                            .state
                            .certificates
                            .read()
                            .get(*domain)
                            .is_none_or(|cert| cert.needs_renewal(now()))
                        && retries
                            .domains
                            .get(*domain)
                            .is_none_or(|retry| retry.ready(now()))
                })
                .cloned()
                .collect();
            // Prioritize imminent expiry; deterministic ties keep scheduling testable.
            needed.sort_by_key(|domain| (self.expiry(domain).unwrap_or(0), domain.clone()));
            if needed.is_empty() {
                continue;
            }
            if let Err(error) = cooldown
                .persist()
                .and_then(|()| self.storage.write("retries.json", &retries))
            {
                tracing::error!(%error, "cannot persist retry state; pausing new certificate requests");
                continue;
            }
            if account.is_none() && retries.account.ready(now()) {
                retries.account.begin(
                    now(),
                    needed.iter().filter_map(|domain| self.expiry(domain)).min(),
                    30,
                );
                if let Err(error) = self.storage.write("retries.json", &retries) {
                    tracing::error!(%error, "cannot persist account retry; pausing registration");
                    continue;
                }
                match tokio::time::timeout(Duration::from_secs(30), self.account(cooldown.clone()))
                    .await
                {
                    Ok(Ok((created, saved))) => {
                        account = Some(created);
                        credentials = saved;
                        retries.account = Default::default();
                        save_at = 0;
                    }
                    result => {
                        let error = match result {
                            Ok(Err(error)) => format!("{error:#}"),
                            _ => "ACME account request timed out".to_owned(),
                        };
                        retries.account.failed(
                            now(),
                            needed.iter().filter_map(|domain| self.expiry(domain)).min(),
                        );
                        tracing::error!(%error, "cannot initialize certificate account");
                    }
                }
                if let Err(error) = self.storage.write("retries.json", &retries) {
                    tracing::error!(%error, "cannot save account retry state");
                }
                continue;
            }
            let Some(account) = &account else {
                continue;
            };
            for domain in needed.into_iter().take(4 - workers.jobs.len()) {
                retries.domains.entry(domain.clone()).or_default().begin(
                    now(),
                    self.expiry(&domain),
                    180,
                );
                if let Err(error) = self.storage.write("retries.json", &retries) {
                    tracing::error!(%domain, %error, "cannot persist retry; pausing issuance");
                    break;
                }
                let account = account.clone();
                let state = self.state.clone();
                let name = domain.clone();
                workers.spawn(domain, async move {
                    tracing::info!(domain = %name, "obtaining HTTPS certificate");
                    let pem = tokio::time::timeout(
                        Duration::from_secs(180),
                        issue(&account, &name, state),
                    )
                    .await
                    .context("certificate issuance timed out")??;
                    PendingCertificate::parse(&name, pem)
                });
            }
        }
    }

    fn expiry(&self, domain: &str) -> Option<i64> {
        self.state
            .certificates
            .read()
            .get(domain)
            .map(|cert| cert.expires_at())
    }
}

#[derive(Default)]
struct Workers {
    jobs: tokio::task::JoinSet<anyhow::Result<PendingCertificate>>,
    active: HashMap<String, tokio::task::AbortHandle>,
}

impl Workers {
    fn spawn(
        &mut self,
        domain: String,
        task: impl Future<Output = anyhow::Result<PendingCertificate>> + Send + 'static,
    ) {
        self.active.insert(domain, self.jobs.spawn(task));
    }

    fn remove(&mut self, id: tokio::task::Id) -> Option<String> {
        let domain = self
            .active
            .iter()
            .find(|(_, handle)| handle.id() == id)
            .map(|(name, _)| name.clone())?;
        self.active.remove(&domain);
        Some(domain)
    }

    fn retain(&mut self, domains: &HashSet<String>) {
        self.active.retain(|domain, handle| {
            if domains.contains(domain) {
                return true;
            }
            handle.abort();
            false
        });
    }
}

struct PendingCertificate {
    pem: CertificatePem,
    certificate: Arc<Certificate>,
}

impl PendingCertificate {
    fn parse(domain: &str, pem: CertificatePem) -> anyhow::Result<Self> {
        let certificate = Arc::new(Certificate::parse(domain, &pem)?);
        ensure!(
            certificate.valid_at(now()),
            "CA returned a certificate that is not currently valid"
        );
        Ok(Self { pem, certificate })
    }

    fn install(&self, domain: &str, storage: &Storage, state: &State) -> anyhow::Result<()> {
        storage.write(&format!("{domain}.json"), &self.pem)?;
        state
            .certificates
            .write()
            .insert(domain.to_owned(), self.certificate.clone());
        Ok(())
    }
}

async fn issue(
    account: &Account,
    domain: &str,
    state: Arc<State>,
) -> anyhow::Result<CertificatePem> {
    // Drop removes challenges even if the request times out or shutdown cancels it.
    let mut cleanup = Challenges {
        state,
        keys: Vec::new(),
    };
    let identifiers = [Identifier::Dns(domain.to_owned())];
    let mut order = account.new_order(&NewOrder::new(&identifiers)).await?;
    let mut authorizations = order.authorizations();
    while let Some(result) = authorizations.next().await {
        let mut authorization = result?;
        match authorization.status {
            AuthorizationStatus::Valid => continue,
            AuthorizationStatus::Pending => {}
            status => bail!("unexpected authorization status: {status:?}"),
        }
        let mut challenge = authorization
            .challenge(ChallengeType::Http01)
            .context("CA did not offer an HTTP-01 challenge")?;
        let key = (domain.to_owned(), challenge.token.clone());
        cleanup.state.challenges.write().insert(
            key.clone(),
            challenge.key_authorization().as_str().to_owned(),
        );
        cleanup.keys.push(key);
        challenge.set_ready().await?;
    }
    ensure!(
        order.poll_ready(&RetryPolicy::default()).await? == OrderStatus::Ready,
        "certificate order was not approved"
    );
    let private_key = order.finalize().await?;
    let chain = order.poll_certificate(&RetryPolicy::default()).await?;
    Ok(CertificatePem { chain, private_key })
}

struct Challenges {
    state: Arc<State>,
    keys: Vec<(String, String)>,
}

impl Drop for Challenges {
    fn drop(&mut self) {
        let mut challenges = self.state.challenges.write();
        for key in &self.keys {
            challenges.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_failure_keeps_old_certificate_and_retries_same_replacement() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let directory = root.path().join("state");
        let storage = Storage::open(&directory, "https://ca.test")?;
        let state = State::new(Config::parse("example.com { respond 200 }")?);
        let old = PendingCertificate::parse(
            "example.com",
            crate::certificates::tests::fixture("example.com", now() - 100, now() + 100)?,
        )?;
        old.install("example.com", &storage, &state)?;
        let replacement = PendingCertificate::parse(
            "example.com",
            crate::certificates::tests::fixture("example.com", now(), now() + 300)?,
        )?;
        let unavailable = root.path().join("unavailable");
        std::fs::rename(&directory, &unavailable)?;
        assert!(
            replacement
                .install("example.com", &storage, &state)
                .is_err()
        );
        assert!(Arc::ptr_eq(
            &state.certificates.read()["example.com"],
            &old.certificate
        ));
        std::fs::rename(&unavailable, &directory)?;
        assert_eq!(
            storage
                .read::<CertificatePem>("example.com.json")?
                .expect("old saved certificate")
                .chain,
            old.pem.chain
        );
        replacement.install("example.com", &storage, &state)?;
        assert!(Arc::ptr_eq(
            &state.certificates.read()["example.com"],
            &replacement.certificate
        ));
        assert_eq!(
            storage
                .read::<CertificatePem>("example.com.json")?
                .expect("replacement")
                .chain,
            replacement.pem.chain
        );
        Ok(())
    }

    #[tokio::test]
    async fn removing_a_site_cancels_its_worker_and_cleans_challenges() -> anyhow::Result<()> {
        let state = Arc::new(State::new(Config::parse("example.com { respond 200 }")?));
        let key = ("example.com".to_owned(), "token".to_owned());
        state
            .challenges
            .write()
            .insert(key.clone(), "response".into());
        let cleanup = Challenges {
            state: state.clone(),
            keys: vec![key],
        };
        let mut workers = Workers::default();
        workers.spawn("example.com".into(), async move {
            let _cleanup = cleanup;
            std::future::pending().await
        });
        tokio::task::yield_now().await;
        workers.retain(&HashSet::new());
        let error = workers
            .jobs
            .join_next()
            .await
            .expect("worker")
            .err()
            .expect("cancelled");
        assert!(error.is_cancelled());
        assert!(state.challenges.read().is_empty());
        assert!(workers.active.is_empty());
        Ok(())
    }

    #[test]
    fn cancelled_challenges_are_removed() -> anyhow::Result<()> {
        let state = Arc::new(State::new(Config::parse(
            "example.com { reverse_proxy localhost:3000 }",
        )?));
        let key = ("example.com".to_owned(), "token".to_owned());
        state
            .challenges
            .write()
            .insert(key.clone(), "response".to_owned());
        drop(Challenges {
            state: state.clone(),
            keys: vec![key],
        });
        assert!(state.challenges.read().is_empty());
        Ok(())
    }
}
