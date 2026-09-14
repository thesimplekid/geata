use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Unix deadlines survive process restarts. Count attempts before contacting the CA.
#[derive(Default, Serialize, Deserialize)]
pub struct Retries {
    pub account: Retry,
    pub domains: HashMap<String, Retry>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Retry {
    failures: u32,
    pub next: i64,
}

impl Retry {
    pub fn ready(&self, now: i64) -> bool {
        now >= self.next
    }

    pub fn begin(&mut self, now: i64, expiry: Option<i64>, timeout: i64) {
        self.failures = self.failures.saturating_add(1);
        self.failed(now, expiry);
        // A crash during issuance must not cause immediate duplicate orders.
        self.next = self.next.max(now.saturating_add(timeout));
    }

    pub fn failed(&mut self, now: i64, expiry: Option<i64>) {
        let base = (30_i64 * 2_i64.pow(self.failures.saturating_sub(1).min(12))).min(86400);
        let mut random = [0_u8; 2];
        let _ = openssl::rand::rand_bytes(&mut random);
        let jitter = i64::from(u16::from_ne_bytes(random)) % (base / 5 + 1);
        // Retry sooner as expiry approaches, without an unbounded expired-cert loop.
        let cap = expiry.map_or(86400, |end| ((end.saturating_sub(now)) / 3).clamp(5, 86400));
        self.next = now.saturating_add((base + jitter).min(cap));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;

    #[test]
    fn retries_survive_restart_and_crashes() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("state");
        let store = Storage::open(&path, "https://ca.test")?;
        let mut retries = Retries::default();
        let retry = retries.domains.entry("example.com".into()).or_default();
        retry.begin(1000, None, 180);
        store.write("retries.json", &retries)?;
        drop(store);
        let store = Storage::open(&path, "https://ca.test")?;
        let mut loaded: Retries = store.read("retries.json")?.expect("saved retries");
        let retry = loaded.domains.get_mut("example.com").expect("saved domain");
        assert!(!retry.ready(1179));
        assert!(retry.ready(1180));
        retry.begin(1180, None, 180);
        retry.failed(1185, None);
        assert!((1245..=1257).contains(&retry.next)); // second failure, not first
        Ok(())
    }

    #[test]
    fn retries_respect_expiry_and_cap_during_long_outages() {
        let mut retry = Retry::default();
        retry.begin(1000, None, 180);
        retry.failed(1000, None);
        assert!((1030..=1036).contains(&retry.next));
        for _ in 0..50 {
            retry.begin(1000, None, 180);
        }
        retry.failed(1000, None);
        assert_eq!(retry.next, 87400);
        retry.failed(1000, Some(1060));
        assert_eq!(retry.next, 1020);
        retry.failed(1000, Some(999));
        assert_eq!(retry.next, 1005);
    }
}
