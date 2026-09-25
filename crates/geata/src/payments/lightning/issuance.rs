//! Bound receiver RPC concurrency without retaining idle receiver entries.
use super::ReceiverConfig;
use crate::config::{Capacity, CapacityPermit};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Weak},
};

pub(super) struct Issuance {
    global: Capacity,
    receivers: parking_lot::Mutex<HashMap<String, Weak<Capacity>>>,
}

pub(super) struct Permit {
    // Release the per-client slot before the last strong receiver reference.
    _receiver_permit: CapacityPermit,
    _receiver: Arc<Capacity>,
    _global: CapacityPermit,
}

impl Default for Issuance {
    fn default() -> Self {
        let mut global = Capacity::new(64);
        global.per_client = 8;
        Self {
            global,
            receivers: Default::default(),
        }
    }
}

impl Issuance {
    pub(super) fn acquire(
        &self,
        receiver: &ReceiverConfig,
        client: IpAddr,
    ) -> anyhow::Result<Permit> {
        use anyhow::Context;
        let global = self
            .global
            .try_acquire(client)
            .context("invoice issuance at capacity")?;
        // Configuration aliases and credential rotation share the endpoint's budget.
        let key = match receiver {
            ReceiverConfig::Ldk(node) => format!("ldk:{}", url::Url::parse(&node.endpoint)?),
            ReceiverConfig::Cashu { mint, .. } => format!("cashu:{mint}"),
        };
        let mut receivers = self.receivers.lock();
        receivers.retain(|_, capacity| capacity.strong_count() != 0);
        let capacity = receivers
            .get(&key)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let mut capacity = Capacity::new(8);
                capacity.per_client = 2;
                let capacity = Arc::new(capacity);
                receivers.insert(key, Arc::downgrade(&capacity));
                capacity
            });
        let permit = capacity
            .try_acquire(client)
            .context("receiver invoice issuance at capacity")?;
        Ok(Permit {
            _receiver_permit: permit,
            _receiver: capacity,
            _global: global,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_and_receiver_limits_are_independent_and_release_on_drop() -> anyhow::Result<()> {
        let issuance = Issuance::default();
        let first = super::super::tests::policy().receiver;
        let mut second = first.clone();
        if let ReceiverConfig::Ldk(node) = &mut second {
            node.endpoint = "https://other.example.com".into();
        }
        let client = [192, 0, 2, 1].into();
        let a = issuance.acquire(&first, client)?;
        let b = issuance.acquire(&first, client)?;
        assert!(issuance.acquire(&first, client).is_err());
        let _other = issuance.acquire(&second, client)?;
        let mut holders = vec![a, b];
        for n in 2..=4 {
            holders.push(issuance.acquire(&first, [192, 0, 2, n].into())?);
            holders.push(issuance.acquire(&first, [192, 0, 2, n].into())?);
        }
        assert!(issuance.acquire(&first, [192, 0, 2, 5].into()).is_err());
        assert!(issuance.acquire(&second, [192, 0, 2, 5].into()).is_ok());
        drop(holders);
        assert!(issuance.acquire(&first, client).is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_releases_receiver_slots_and_aliases_share_limits() -> anyhow::Result<()> {
        let issuance = Arc::new(Issuance::default());
        let receiver = super::super::tests::policy().receiver;
        let mut alias = receiver.clone();
        if let ReceiverConfig::Ldk(node) = &mut alias {
            node.macaroon_file = "/rotated/key".into();
        }
        let client = [192, 0, 2, 1].into();
        let first = issuance.acquire(&receiver, client)?;
        let second = issuance.acquire(&alias, client)?;
        assert!(issuance.acquire(&receiver, client).is_err());
        let task = tokio::spawn(async move {
            let _permit = first;
            std::future::pending::<()>().await;
        });
        task.abort();
        assert!(task.await.expect_err("cancelled").is_cancelled());
        assert!(issuance.acquire(&alias, client).is_ok());
        drop(second);
        Ok(())
    }

    #[test]
    fn global_capacity_and_receiver_metadata_are_bounded() -> anyhow::Result<()> {
        let issuance = Issuance::default();
        let mut holders = Vec::new();
        for n in 1..=64 {
            let mut receiver = super::super::tests::policy().receiver;
            if let ReceiverConfig::Ldk(node) = &mut receiver {
                node.endpoint = format!("https://receiver-{n}.example.com");
            }
            holders.push(issuance.acquire(&receiver, [192, 0, 2, n].into())?);
        }
        let receiver = super::super::tests::policy().receiver;
        assert!(issuance.acquire(&receiver, [192, 0, 2, 65].into()).is_err());
        drop(holders);
        let _permit = issuance.acquire(&receiver, [192, 0, 2, 65].into())?;
        assert_eq!(issuance.receivers.lock().len(), 1);
        Ok(())
    }
}
