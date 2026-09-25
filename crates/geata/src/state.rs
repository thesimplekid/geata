use std::{collections::HashMap, sync::Arc};

use arc_swap::ArcSwap;
use parking_lot::RwLock;

use crate::{certificates::Certificate, config::Config, payments::Payments};

pub struct State {
    pub config: ArcSwap<Config>,
    pub payments: Option<Arc<Payments>>,
    pub lightning: Option<Arc<crate::payments::lightning::Lightning>>,
    pub certificates: RwLock<HashMap<String, Arc<Certificate>>>,
    pub challenges: RwLock<HashMap<(String, String), String>>,
}

impl State {
    pub fn new(config: Config) -> Self {
        Self {
            config: ArcSwap::from_pointee(config),
            payments: None,
            lightning: None,
            certificates: RwLock::new(HashMap::new()),
            challenges: RwLock::new(HashMap::new()),
        }
    }

    pub fn replace_config(&self, mut config: Config) {
        let previous = self.config.load();
        for (domain, site) in &mut config.sites {
            if let Some(old) = previous.sites.get(domain)
                && site.payment_verification.limit == old.payment_verification.limit
                && site.payment_verification.ipv6_prefix == old.payment_verification.ipv6_prefix
            {
                site.payment_verification = old.payment_verification.clone();
            }
            if let (Some(next), Some(old)) = (
                &site.capacity,
                previous
                    .sites
                    .get(domain)
                    .and_then(|site| site.capacity.as_ref()),
            ) && next.max == old.max
                && next.ipv6_prefix == old.ipv6_prefix
            {
                site.capacity = Some(old.clone());
            }
            if let (Some(next), Some(old)) = (
                site.rate_limit.as_ref(),
                previous
                    .sites
                    .get(domain)
                    .and_then(|site| site.rate_limit.as_ref()),
            ) && next.limit == old.limit
                && next.ipv6_prefix == old.ipv6_prefix
            {
                site.rate_limit = Some(Arc::clone(old));
            }
        }
        self.config.store(Arc::new(config));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_preserves_payment_attempts_and_respects_prefix_changes() -> anyhow::Result<()> {
        let parse = |prefix| {
            Config::parse(&format!(
                "http://localhost {{ respond ok ipv6_prefix {prefix} }}"
            ))
        };
        let state = State::new(parse(64)?);
        let old = state.config.load().sites["localhost"]
            .payment_verification
            .clone();
        state.replace_config(parse(64)?);
        assert!(Arc::ptr_eq(
            &old,
            &state.config.load().sites["localhost"].payment_verification
        ));
        state.replace_config(parse(48)?);
        assert!(!Arc::ptr_eq(
            &old,
            &state.config.load().sites["localhost"].payment_verification
        ));
        Ok(())
    }

    #[test]
    fn reload_preserves_unchanged_limits_and_drops_removed_or_changed_limits() -> anyhow::Result<()>
    {
        let parse = |limit: &str, body: &str| {
            Config::parse(&format!(
                "http://localhost {{\n{limit}\nrespond \"{body}\"\n}}"
            ))
        };
        let state = State::new(parse("rate_limit 1/s burst 2", "old")?);
        let original = Arc::clone(
            state.config.load().sites["localhost"]
                .rate_limit
                .as_ref()
                .expect("limiter"),
        );
        state.replace_config(parse("rate_limit 1/s burst 2", "new")?);
        assert!(Arc::ptr_eq(
            &original,
            state.config.load().sites["localhost"]
                .rate_limit
                .as_ref()
                .expect("limiter")
        ));
        state.replace_config(parse("rate_limit 1/s burst 3", "new")?);
        assert!(!Arc::ptr_eq(
            &original,
            state.config.load().sites["localhost"]
                .rate_limit
                .as_ref()
                .expect("limiter")
        ));
        state.replace_config(parse("", "new")?);
        assert!(state.config.load().sites["localhost"].rate_limit.is_none());
        state.replace_config(parse("rate_limit 1/s burst 2", "new")?);
        assert!(!Arc::ptr_eq(
            &original,
            state.config.load().sites["localhost"]
                .rate_limit
                .as_ref()
                .expect("limiter")
        ));
        Ok(())
    }
}
