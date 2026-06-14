use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Async token-bucket rate limiter. One bucket per provider: tokens refill at
/// `max_per_minute / 60` per second, capped at `max_per_minute`. `acquire`
/// consumes one token, sleeping if the bucket is empty.
pub struct RateLimiter {
    max_per_minute: u32,
    inner: Mutex<Bucket>,
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            max_per_minute,
            inner: Mutex::new(Bucket {
                tokens: max_per_minute as f64,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Acquire one request slot, sleeping asynchronously if the bucket is empty.
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut b = self.inner.lock().unwrap();
                let now = Instant::now();
                let elapsed = now.duration_since(b.last_refill).as_secs_f64();
                let refill_per_sec = self.max_per_minute as f64 / 60.0;
                b.tokens = (b.tokens + elapsed * refill_per_sec).min(self.max_per_minute as f64);
                b.last_refill = now;
                if b.tokens >= 1.0 {
                    b.tokens -= 1.0;
                    return;
                }
                let need = 1.0 - b.tokens;
                let secs = (need / refill_per_sec).max(0.001);
                Duration::from_secs_f64(secs)
            };
            tokio::time::sleep(wait).await;
        }
    }
}
