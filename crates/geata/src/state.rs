use std::{collections::HashMap, sync::Arc};

use arc_swap::ArcSwap;
use parking_lot::RwLock;

use crate::{certificates::Certificate, config::Config};

pub struct State {
    pub config: ArcSwap<Config>,
    pub certificates: RwLock<HashMap<String, Arc<Certificate>>>,
    pub challenges: RwLock<HashMap<(String, String), String>>,
}

impl State {
    pub fn new(config: Config) -> Self {
        Self {
            config: ArcSwap::from_pointee(config),
            certificates: RwLock::new(HashMap::new()),
            challenges: RwLock::new(HashMap::new()),
        }
    }

    pub fn replace_config(&self, mut config: Config) {
        let previous = self.config.load();
        for (domain, site) in &mut config.sites {
            if let (Some(next), Some(old)) = (
                site.rate_limit.as_ref(),
                previous
                    .sites
                    .get(domain)
                    .and_then(|site| site.rate_limit.as_ref()),
            ) && next.limit == old.limit
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
