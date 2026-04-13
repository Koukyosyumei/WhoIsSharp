//! In-memory TTL cache for raw HTTP response bodies.
//!
//! Keyed by URL string; values are raw JSON text. Using text (not parsed
//! types) means the cache is independent of any struct changes, and we pay
//! the parse cost only once per cache miss.
//!
//! Thread-safe via `tokio::sync::Mutex`; designed to be shared behind an `Arc`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

// ─── TtlCache ─────────────────────────────────────────────────────────────────

pub struct TtlCache {
    store: Mutex<HashMap<String, CacheEntry>>,
    ttl:   Duration,
}

struct CacheEntry {
    inserted_at: Instant,
    body:        String,
}

impl TtlCache {
    pub fn new(ttl_secs: u64) -> Self {
        TtlCache {
            store: Mutex::new(HashMap::new()),
            ttl:   Duration::from_secs(ttl_secs),
        }
    }

    /// Return the cached body if the entry exists and has not expired.
    pub async fn get(&self, key: &str) -> Option<String> {
        let store = self.store.lock().await;
        store.get(key).and_then(|e| {
            if e.inserted_at.elapsed() < self.ttl {
                Some(e.body.clone())
            } else {
                None
            }
        })
    }

    /// Insert or overwrite a cache entry.
    pub async fn set(&self, key: impl Into<String>, body: String) {
        self.store.lock().await.insert(
            key.into(),
            CacheEntry { inserted_at: Instant::now(), body },
        );
    }

    /// Remove all expired entries to keep memory bounded.
    pub async fn evict_expired(&self) {
        let ttl = self.ttl;
        self.store
            .lock()
            .await
            .retain(|_, e| e.inserted_at.elapsed() < ttl);
    }

    /// Number of live (non-expired) entries.
    #[cfg(test)]
    pub async fn len(&self) -> usize {
        let store = self.store.lock().await;
        let ttl = self.ttl;
        store.values().filter(|e| e.inserted_at.elapsed() < ttl).count()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cache_hit_and_miss() {
        let cache = TtlCache::new(60);
        assert!(cache.get("key").await.is_none());
        cache.set("key", "value".to_string()).await;
        assert_eq!(cache.get("key").await.unwrap(), "value");
    }

    #[tokio::test]
    async fn cache_overwrites_existing() {
        let cache = TtlCache::new(60);
        cache.set("k", "v1".to_string()).await;
        cache.set("k", "v2".to_string()).await;
        assert_eq!(cache.get("k").await.unwrap(), "v2");
    }

    #[tokio::test]
    async fn expired_entry_returns_none() {
        // TTL = 0 seconds → every entry is immediately expired
        let cache = TtlCache::new(0);
        cache.set("k", "v".to_string()).await;
        // Sleep 1ms so elapsed() > 0
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        assert!(cache.get("k").await.is_none());
    }

    #[tokio::test]
    async fn evict_removes_expired() {
        let cache = TtlCache::new(0);
        cache.set("a", "1".to_string()).await;
        cache.set("b", "2".to_string()).await;
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        cache.evict_expired().await;
        assert_eq!(cache.len().await, 0);
    }

    #[tokio::test]
    async fn different_keys_are_independent() {
        let cache = TtlCache::new(60);
        cache.set("a", "1".to_string()).await;
        cache.set("b", "2".to_string()).await;
        assert_eq!(cache.get("a").await.unwrap(), "1");
        assert_eq!(cache.get("b").await.unwrap(), "2");
        assert!(cache.get("c").await.is_none());
    }
}
