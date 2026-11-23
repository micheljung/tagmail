//! Login rate limits and the client IP behind trusted reverse proxies.
//!
//! Per-client buckets do the limiting. The global bucket is only a high
//! backstop against attacks spread over many addresses: it is far above
//! anything one owner needs, so a single attacker cannot exhaust it, while
//! the single-flight password hasher already bounds CPU and memory. The
//! trade-off: an attacker with hundreds of addresses can still delay the
//! owner's password login until the backstop refills, and the owner is
//! never exempt, since a login request carries no proof of who sends it.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use ipnet::IpNet;

#[derive(Debug, Clone, Copy)]
pub struct Rate {
    pub burst: u32,
    /// One attempt is regained per `refill`.
    pub refill: Duration,
}

/// 10 attempts, then one every 10 s per client.
pub const PER_IP: Rate = Rate {
    burst: 10,
    refill: Duration::from_secs(10),
};
/// 600 attempts, then ten per second across all clients.
pub const GLOBAL: Rate = Rate {
    burst: 600,
    refill: Duration::from_millis(100),
};
/// Account tests log in to arbitrary servers: 10 a minute in all (the
/// API is owner-only, so one bucket).
pub const ACCOUNT_TESTS: Rate = Rate {
    burst: 10,
    refill: Duration::from_secs(6),
};
/// Tracked clients; beyond this, full buckets are dropped first, and new
/// clients that still do not fit only count against the global bucket.
const MAX_CLIENTS: usize = 4096;

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn full(rate: Rate, now: Instant) -> Bucket {
        Bucket {
            tokens: f64::from(rate.burst),
            at: now,
        }
    }

    fn refill(&mut self, rate: Rate, now: Instant) {
        let regained = now.duration_since(self.at).as_secs_f64() / rate.refill.as_secs_f64();
        self.tokens = (self.tokens + regained).min(f64::from(rate.burst));
        self.at = now;
    }

    fn wait(&self, rate: Rate) -> Duration {
        rate.refill.mul_f64((1.0 - self.tokens).max(0.0))
    }
}

pub struct RateLimiter {
    per_ip: Rate,
    global: Rate,
    state: Mutex<(Bucket, HashMap<IpAddr, Bucket>)>,
}

impl RateLimiter {
    pub fn new(per_ip: Rate, global: Rate) -> RateLimiter {
        RateLimiter {
            per_ip,
            global,
            state: Mutex::new((Bucket::full(global, Instant::now()), HashMap::new())),
        }
    }

    /// Takes one attempt for `ip`, or returns how long to wait.
    pub fn check(&self, ip: IpAddr) -> Result<(), Duration> {
        self.check_at(ip, Instant::now())
    }

    fn check_at(&self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        let key = client_key(ip);
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (global, clients) = &mut *guard;
        if clients.len() >= MAX_CLIENTS && !clients.contains_key(&key) {
            let rate = self.per_ip;
            clients.retain(|_, b| {
                b.refill(rate, now);
                b.tokens < f64::from(rate.burst)
            });
        }
        let mut client = if clients.len() < MAX_CLIENTS || clients.contains_key(&key) {
            let client = clients
                .entry(key)
                .or_insert_with(|| Bucket::full(self.per_ip, now));
            client.refill(self.per_ip, now);
            if client.tokens < 1.0 {
                return Err(client.wait(self.per_ip));
            }
            Some(client)
        } else {
            None
        };
        global.refill(self.global, now);
        if global.tokens < 1.0 {
            return Err(global.wait(self.global));
        }
        if let Some(client) = client.as_mut() {
            client.tokens -= 1.0;
        }
        global.tokens -= 1.0;
        Ok(())
    }
}

/// IPv6 clients usually own a whole /64.
fn client_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let mut segments = v6.segments();
                segments[4..].fill(0);
                IpAddr::V6(Ipv6Addr::from(segments))
            }
        },
    }
}

/// The client's address: the peer, unless the peer is a trusted proxy. Then
/// `X-Forwarded-For` is read right to left and the first address that is
/// not a trusted proxy is the client.
pub fn client_ip(peer: IpAddr, headers: &HeaderMap, trusted: &[IpNet]) -> IpAddr {
    let is_trusted = |ip: &IpAddr| trusted.iter().any(|net| net.contains(ip));
    if !is_trusted(&peer) {
        return peer;
    }
    let mut hops: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .collect();
    let mut client = peer;
    while let Some(hop) = hops.pop() {
        let Ok(ip) = hop.parse::<IpAddr>() else {
            return client;
        };
        client = ip;
        if !is_trusted(&ip) {
            break;
        }
    }
    client
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn forwarded_for_is_only_trusted_from_listed_proxies() {
        let trusted: Vec<IpNet> = vec!["10.0.0.0/8".parse().unwrap()];
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.1.1.1, 2.2.2.2"),
        );
        headers.append("x-forwarded-for", HeaderValue::from_static("10.0.0.7"));
        assert_eq!(client_ip(ip("9.9.9.9"), &headers, &trusted), ip("9.9.9.9"));
        assert_eq!(client_ip(ip("10.0.0.1"), &headers, &trusted), ip("2.2.2.2"));
        assert_eq!(client_ip(ip("10.0.0.1"), &headers, &[]), ip("10.0.0.1"));
        assert_eq!(
            client_ip(ip("10.0.0.1"), &HeaderMap::new(), &trusted),
            ip("10.0.0.1")
        );
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("garbage, 10.1.1.1"),
        );
        assert_eq!(
            client_ip(ip("10.0.0.1"), &headers, &trusted),
            ip("10.1.1.1")
        );
    }

    #[test]
    fn limits_per_client_and_globally() {
        let per_ip = Rate {
            burst: 2,
            refill: Duration::from_secs(10),
        };
        let global = Rate {
            burst: 3,
            refill: Duration::from_secs(1),
        };
        let limiter = RateLimiter::new(per_ip, global);
        let t = Instant::now();
        assert!(limiter.check_at(ip("1.1.1.1"), t).is_ok());
        assert!(limiter.check_at(ip("1.1.1.1"), t).is_ok());
        let wait = limiter.check_at(ip("1.1.1.1"), t).unwrap_err();
        assert_eq!(wait, Duration::from_secs(10));
        assert!(limiter.check_at(ip("2.2.2.2"), t).is_ok());
        assert!(
            limiter.check_at(ip("3.3.3.3"), t).is_err(),
            "global bucket empty"
        );
        let later = t + Duration::from_secs(10);
        assert!(limiter.check_at(ip("1.1.1.1"), later).is_ok());
    }

    #[test]
    fn a_full_client_table_falls_back_to_the_global_bucket() {
        let per_ip = Rate {
            burst: 1,
            refill: Duration::from_secs(3600),
        };
        let global = Rate {
            burst: 2 * MAX_CLIENTS as u32,
            refill: Duration::from_secs(1),
        };
        let limiter = RateLimiter::new(per_ip, global);
        let t = Instant::now();
        for n in 0..MAX_CLIENTS as u32 {
            let ip = IpAddr::V4(std::net::Ipv4Addr::from(n + 1));
            assert!(limiter.check_at(ip, t).is_ok());
        }
        assert!(
            limiter.check_at(ip("203.0.113.9"), t).is_ok(),
            "an untracked client is not locked out by a full table"
        );
    }

    #[test]
    fn ipv6_clients_share_their_slash_64() {
        assert_eq!(client_key(ip("2001:db8:1:2:3::4")), ip("2001:db8:1:2::"));
        assert_eq!(client_key(ip("::ffff:1.2.3.4")), ip("1.2.3.4"));
    }
}
