use std::{
    collections::{HashMap, hash_map::RandomState},
    hash::BuildHasher,
    net::IpAddr,
    time::{Duration, Instant},
};

use parking_lot::Mutex;

const SHARDS: usize = 16;
const CLIENTS_PER_SHARD: usize = 1024;
const IDLE: Duration = Duration::from_secs(60);
const TOKEN: u128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    pub per_second: u32,
    pub burst: u32,
}

/// One site's buckets, shared by every worker and retained across unchanged reloads.
#[derive(Debug)]
pub struct RateLimiter {
    pub limit: Limit,
    hash: RandomState,
    shards: [Mutex<Shard>; SHARDS],
}

#[derive(Debug)]
struct Shard {
    clients: HashMap<IpAddr, Bucket>,
    swept: Instant,
}

#[derive(Debug)]
struct Bucket {
    // Fixed-point tokens retain fractional refill without floating-point rounding.
    credit: u128,
    updated: Instant,
}

impl RateLimiter {
    pub fn new(limit: Limit) -> Self {
        Self {
            limit,
            hash: RandomState::new(),
            shards: std::array::from_fn(|_| {
                Mutex::new(Shard {
                    clients: HashMap::new(),
                    swept: Instant::now(),
                })
            }),
        }
    }

    pub fn check(&self, client: IpAddr) -> Result<(), Duration> {
        self.check_with_clock(client, Instant::now)
    }

    fn check_with_clock(
        &self,
        client: IpAddr,
        clock: impl FnOnce() -> Instant,
    ) -> Result<(), Duration> {
        // An IPv4 address must keep the same bucket on a dual-stack listener.
        let client = client.to_canonical();
        let index = (self.hash.hash_one(client) % SHARDS as u64) as usize;
        let mut shard = self.shards[index].lock();
        // Sample after acquiring the lock so concurrent requests cannot move time backwards.
        shard.check(client, self.limit, clock())
    }
}

impl Shard {
    fn check(&mut self, client: IpAddr, limit: Limit, now: Instant) -> Result<(), Duration> {
        let capacity = u128::from(limit.burst) * TOKEN;
        if now.duration_since(self.swept) >= IDLE {
            self.clients.retain(|_, bucket| {
                let elapsed = now.duration_since(bucket.updated);
                // Only remove idle buckets that would already be completely refilled.
                elapsed < IDLE || bucket.refilled(limit, now) < capacity
            });
            self.swept = now;
        }
        if !self.clients.contains_key(&client) && self.clients.len() >= CLIENTS_PER_SHARD {
            // Never evict an active bucket: rotating IPs must not reset other allowances.
            return Err(IDLE);
        }
        let bucket = self.clients.entry(client).or_insert(Bucket {
            credit: capacity,
            updated: now,
        });
        bucket.credit = bucket.refilled(limit, now);
        bucket.updated = now;
        if bucket.credit >= TOKEN {
            bucket.credit -= TOKEN;
            Ok(())
        } else {
            let nanos = (TOKEN - bucket.credit).div_ceil(u128::from(limit.per_second));
            Err(Duration::from_nanos(nanos as u64))
        }
    }
}

impl Bucket {
    fn refilled(&self, limit: Limit, now: Instant) -> u128 {
        (self.credit + now.duration_since(self.updated).as_nanos() * u128::from(limit.per_second))
            .min(u128::from(limit.burst) * TOKEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

    #[test]
    fn burst_refills_fractionally_and_caps_after_idle() {
        let limiter = RateLimiter::new(Limit {
            per_second: 10,
            burst: 2,
        });
        let now = Instant::now();
        let check = |millis| limiter.check_with_clock(IP, || now + Duration::from_millis(millis));
        assert_eq!(check(0), Ok(()));
        assert_eq!(check(0), Ok(()));
        assert_eq!(check(0), Err(Duration::from_millis(100)));
        assert_eq!(check(40), Err(Duration::from_millis(60)));
        assert_eq!(check(99), Err(Duration::from_millis(1)));
        assert_eq!(check(100), Ok(()));
        assert_eq!(check(100), Err(Duration::from_millis(100)));
        assert_eq!(check(120_000), Ok(()));
        assert_eq!(check(120_000), Ok(()));
        assert_eq!(check(120_000), Err(Duration::from_millis(100)));
    }

    #[test]
    fn clients_are_independent_and_ipv4_mapped_addresses_share_a_bucket() {
        let limiter = RateLimiter::new(Limit {
            per_second: 1,
            burst: 1,
        });
        let now = Instant::now();
        let mapped = IpAddr::V6(std::net::Ipv4Addr::LOCALHOST.to_ipv6_mapped());
        let other = IpAddr::V6(std::net::Ipv6Addr::LOCALHOST);
        assert!(limiter.check_with_clock(IP, || now).is_ok());
        assert!(limiter.check_with_clock(mapped, || now).is_err());
        assert!(limiter.check_with_clock(other, || now).is_ok());
    }

    #[test]
    fn workers_share_one_allowance() {
        let limiter = RateLimiter::new(Limit {
            per_second: 1,
            burst: 7,
        });
        let now = Instant::now();
        let barrier = std::sync::Barrier::new(32);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..32)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        limiter.check_with_clock(IP, || now).is_ok()
                    })
                })
                .collect();
            let accepted = workers
                .into_iter()
                .map(|worker| usize::from(worker.join().expect("worker panicked")))
                .sum::<usize>();
            assert_eq!(accepted, 7);
        });
    }

    #[test]
    fn full_table_preserves_depleted_buckets_and_eventually_reclaims_idle_clients() {
        let now = Instant::now();
        let mut shard = Shard {
            clients: HashMap::new(),
            swept: now,
        };
        // A large burst takes longer than the idle window to refill.
        let limit = Limit {
            per_second: 1,
            burst: 120,
        };
        for ip in 0..CLIENTS_PER_SHARD as u32 {
            shard.clients.insert(
                IpAddr::V4(ip.into()),
                Bucket {
                    credit: 0,
                    updated: now,
                },
            );
        }
        assert_eq!(shard.check(IP, limit, now + IDLE), Err(IDLE));
        assert_eq!(shard.clients.len(), CLIENTS_PER_SHARD);
        assert!(shard.check(IP, limit, now + IDLE * 2).is_ok());
        assert_eq!(shard.clients.len(), 1);
    }
}
