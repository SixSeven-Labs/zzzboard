//! Per-IP token bucket. This is the only admission control on the whole
//! server: no auth, no captcha, no bot detection, no blocking. It exists so a
//! single runaway loop cannot saturate the fsync path for everyone else.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Instant;

use parking_lot::Mutex;

pub struct RateLimiter {
    per_second: f64,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(per_second: f64) -> Self {
        Self {
            per_second,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// True if the request may proceed. Burst == sustained rate.
    pub fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut map = self.buckets.lock();
        if map.len() > 8192 {
            // Buckets refill fully within a second; anything idle for 5 s is
            // indistinguishable from a fresh one, so drop it.
            map.retain(|_, b| now.duration_since(b.last).as_secs_f64() < 5.0);
        }
        let b = map.entry(ip).or_insert(Bucket {
            tokens: self.per_second,
            last: now,
        });
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.per_second).min(self.per_second);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_deny() {
        let rl = RateLimiter::new(200.0);
        let ip: IpAddr = "203.0.113.5".parse().unwrap();
        let allowed = (0..250).filter(|_| rl.allow(ip)).count();
        // 200 from the initial bucket plus whatever trickled in during the loop
        assert!((200..=202).contains(&allowed), "allowed {allowed}");
        let other: IpAddr = "203.0.113.6".parse().unwrap();
        assert!(rl.allow(other), "buckets are per IP");
    }
}
