//! Token-bucket rate limiter for the Tachyon gateway.
//!
//! Provides per-IP rate limiting as an axum middleware layer.
//! Uses a simple token-bucket algorithm with periodic cleanup of stale entries.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tokio::sync::Mutex;

/// Configuration for the rate limiter.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Whether rate limiting is enabled.
    pub enabled: bool,
    /// Maximum sustained requests per second.
    pub requests_per_second: u32,
    /// Maximum burst size (bucket capacity).
    pub burst_size: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        RateLimitConfig {
            enabled: true,
            requests_per_second: 100,
            burst_size: 200,
        }
    }
}

/// A single token bucket for one IP address.
#[derive(Debug, Clone)]
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(burst_size: u32) -> Self {
        TokenBucket {
            tokens: burst_size as f64,
            last_refill: Instant::now(),
        }
    }

    /// Attempts to consume one token. Returns true if allowed, false if rate-limited.
    fn try_consume(&mut self, rate: u32, burst: u32) -> bool {
        self.refill(rate, burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Refill tokens based on elapsed time.
    fn refill(&mut self, rate: u32, burst: u32) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate as f64).min(burst as f64);
        self.last_refill = now;
    }
}

/// Per-IP token bucket rate limiter.
#[derive(Clone)]
pub struct RateLimiter {
    config: RateLimitConfig,
    buckets: Arc<Mutex<HashMap<IpAddr, TokenBucket>>>,
}

impl RateLimiter {
    /// Create a new rate limiter with the given configuration.
    pub fn new(config: RateLimitConfig) -> Self {
        RateLimiter {
            config,
            buckets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Check if a request from the given IP is allowed.
    pub async fn check(&self, ip: IpAddr) -> bool {
        if !self.config.enabled {
            return true;
        }

        let mut buckets = self.buckets.lock().await;
        let bucket = buckets
            .entry(ip)
            .or_insert_with(|| TokenBucket::new(self.config.burst_size));
        bucket.try_consume(self.config.requests_per_second, self.config.burst_size)
    }

    /// Remove stale entries that have been idle for more than the given duration.
    pub async fn cleanup_stale(&self, max_idle_secs: u64) {
        let mut buckets = self.buckets.lock().await;
        let now = Instant::now();
        buckets
            .retain(|_, bucket| now.duration_since(bucket.last_refill).as_secs() < max_idle_secs);
    }

    /// Returns the number of tracked IPs.
    pub async fn tracked_ips(&self) -> usize {
        self.buckets.lock().await.len()
    }
}

/// Axum middleware function for rate limiting.
///
/// Must be used with `axum::middleware::from_fn_with_state`.
pub async fn rate_limit_middleware(
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    axum::extract::State(limiter): axum::extract::State<RateLimiter>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !limiter.check(addr.ip()).await {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            serde_json::json!({"error": "rate limit exceeded"}).to_string(),
        )
            .into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[tokio::test]
    async fn test_rate_limiter_allows_within_limit() {
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 10,
            burst_size: 10,
        };
        let limiter = RateLimiter::new(config);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        // Should allow up to burst_size requests
        for _ in 0..10 {
            assert!(limiter.check(ip).await);
        }
    }

    #[tokio::test]
    async fn test_rate_limiter_rejects_when_exceeded() {
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 10,
            burst_size: 5,
        };
        let limiter = RateLimiter::new(config);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        // Consume all tokens
        for _ in 0..5 {
            assert!(limiter.check(ip).await);
        }

        // Next request should be rejected
        assert!(!limiter.check(ip).await);
    }

    #[tokio::test]
    async fn test_rate_limiter_bucket_refills() {
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 1000,
            burst_size: 5,
        };
        let limiter = RateLimiter::new(config);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        // Consume all tokens
        for _ in 0..5 {
            assert!(limiter.check(ip).await);
        }
        assert!(!limiter.check(ip).await);

        // Wait for refill (1000 rps = 1 token per ms, wait 10ms to get ~10 tokens, capped at 5)
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Should be allowed again after refill
        assert!(limiter.check(ip).await);
    }

    #[tokio::test]
    async fn test_rate_limiter_disabled() {
        let config = RateLimitConfig {
            enabled: false,
            requests_per_second: 1,
            burst_size: 1,
        };
        let limiter = RateLimiter::new(config);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        // Should always allow when disabled
        for _ in 0..100 {
            assert!(limiter.check(ip).await);
        }
    }

    #[tokio::test]
    async fn test_rate_limiter_per_ip_isolation() {
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 10,
            burst_size: 2,
        };
        let limiter = RateLimiter::new(config);
        let ip1 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let ip2 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

        // Exhaust IP1's tokens
        assert!(limiter.check(ip1).await);
        assert!(limiter.check(ip1).await);
        assert!(!limiter.check(ip1).await);

        // IP2 should still have tokens
        assert!(limiter.check(ip2).await);
        assert!(limiter.check(ip2).await);
        assert!(!limiter.check(ip2).await);
    }

    #[tokio::test]
    async fn test_rate_limiter_cleanup_stale() {
        let config = RateLimitConfig {
            enabled: true,
            requests_per_second: 10,
            burst_size: 10,
        };
        let limiter = RateLimiter::new(config);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        // Create an entry
        limiter.check(ip).await;
        assert_eq!(limiter.tracked_ips().await, 1);

        // Cleanup with very short idle time -- entry is fresh so it stays
        limiter.cleanup_stale(60).await;
        assert_eq!(limiter.tracked_ips().await, 1);

        // Cleanup with 0 second idle time -- everything should be removed
        // (wait a tiny bit so the entry is at least 1 nanosecond old)
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        limiter.cleanup_stale(0).await;
        assert_eq!(limiter.tracked_ips().await, 0);
    }
}
