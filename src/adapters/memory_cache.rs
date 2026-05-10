//! In-memory TTL cache backed by [`moka`].
//!
//! Per-entry TTLs are emulated by storing an expiry timestamp alongside
//! each value. moka's built-in `expire_after` is per-cache, not per-entry,
//! so we can't use it directly without surrendering control over policy.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moka::future::Cache as MokaCache;

use crate::ports::{Cache, Clock};

#[derive(Clone)]
struct Entry {
    value: String,
    expires_at: DateTime<Utc>,
}

/// In-memory cache. Bounded to 10k entries — the working set is small
/// (currency list + per-key wallet + per-query wiki search) so this is
/// effectively unbounded for normal use, but caps blast-radius if a
/// misbehaving caller floods cache keys.
pub struct MemoryCache {
    inner: MokaCache<String, Entry>,
    clock: Arc<dyn Clock>,
}

impl MemoryCache {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self::with_capacity(10_000, clock)
    }

    pub fn with_capacity(capacity: u64, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner: MokaCache::builder().max_capacity(capacity).build(),
            clock,
        }
    }
}

#[async_trait]
impl Cache for MemoryCache {
    async fn set(&self, key: &str, value: String, ttl: Duration) {
        let expires_at = self.clock.now()
            + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::days(365));
        self.inner
            .insert(key.to_owned(), Entry { value, expires_at })
            .await;
    }

    async fn get(&self, key: &str) -> Option<String> {
        let entry = self.inner.get(key).await?;
        if self.clock.now() >= entry.expires_at {
            // Lazy eviction — fine because moka also evicts on capacity pressure.
            self.inner.invalidate(key).await;
            return None;
        }
        Some(entry.value)
    }

    async fn delete(&self, key: &str) {
        self.inner.invalidate(key).await;
    }

    async fn len(&self) -> usize {
        // moka's count is approximate; that's fine for diagnostics.
        self.inner.run_pending_tasks().await;
        usize::try_from(self.inner.entry_count()).unwrap_or(usize::MAX)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Test-only clock you can advance manually.
    struct TestClock(Mutex<DateTime<Utc>>);

    impl TestClock {
        fn new() -> Arc<Self> {
            Arc::new(Self(Mutex::new(
                DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            )))
        }
        fn advance(&self, by: Duration) {
            let mut t = self.0.lock().unwrap();
            *t += chrono::Duration::from_std(by).unwrap();
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }

    #[tokio::test]
    async fn set_then_get_returns_value() {
        let clock = TestClock::new();
        let cache = MemoryCache::new(clock.clone());
        cache.set("k", "v".into(), Duration::from_secs(60)).await;
        assert_eq!(cache.get("k").await.as_deref(), Some("v"));
    }

    #[tokio::test]
    async fn miss_returns_none() {
        let cache = MemoryCache::new(TestClock::new());
        assert!(cache.get("absent").await.is_none());
    }

    #[tokio::test]
    async fn expired_entry_returns_none() {
        let clock = TestClock::new();
        let cache = MemoryCache::new(clock.clone());
        cache.set("k", "v".into(), Duration::from_secs(60)).await;
        clock.advance(Duration::from_secs(61));
        assert!(cache.get("k").await.is_none());
    }

    #[tokio::test]
    async fn delete_removes_entry() {
        let clock = TestClock::new();
        let cache = MemoryCache::new(clock);
        cache.set("k", "v".into(), Duration::from_secs(60)).await;
        cache.delete("k").await;
        assert!(cache.get("k").await.is_none());
    }
}
