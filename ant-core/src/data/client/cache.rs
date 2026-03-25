//! LRU chunk cache for reducing redundant network requests.
//!
//! Caches recently fetched chunks in memory to avoid re-fetching
//! the same content-addressed data from the network.

use ant_node::client::XorName;
use bytes::Bytes;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Mutex, PoisonError};

/// Default cache capacity (number of chunks).
const DEFAULT_CACHE_CAPACITY: usize = 1024;

/// An LRU cache for content-addressed chunks.
pub struct ChunkCache {
    inner: Mutex<LruCache<XorName, Bytes>>,
}

impl ChunkCache {
    /// Create a new chunk cache with the given capacity.
    ///
    /// Falls back to the default capacity (1024) if zero is provided.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        // Use provided capacity, falling back to default if zero
        let effective = if capacity == 0 {
            DEFAULT_CACHE_CAPACITY
        } else {
            capacity
        };
        // SAFETY: effective is guaranteed non-zero by the check above
        let cap = NonZeroUsize::new(effective).unwrap_or(NonZeroUsize::MIN);
        Self {
            inner: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Create a cache with the default capacity.
    #[must_use]
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_CACHE_CAPACITY)
    }

    /// Get a chunk from the cache.
    #[must_use]
    pub fn get(&self, address: &XorName) -> Option<Bytes> {
        let mut cache = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        cache.get(address).cloned()
    }

    /// Insert a chunk into the cache.
    pub fn put(&self, address: XorName, content: Bytes) {
        let mut cache = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        cache.put(address, content);
    }

    /// Check if a chunk is in the cache.
    #[must_use]
    pub fn contains(&self, address: &XorName) -> bool {
        let cache = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        cache.contains(address)
    }

    /// Get the number of cached chunks.
    #[must_use]
    pub fn len(&self) -> usize {
        let cache = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        cache.len()
    }

    /// Check if the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove a specific entry from the cache.
    pub fn remove(&self, address: &XorName) {
        let mut cache = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        cache.pop(address);
    }

    /// Clear all cached chunks.
    pub fn clear(&self) {
        let mut cache = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        cache.clear();
    }
}

impl Default for ChunkCache {
    fn default() -> Self {
        Self::with_default_capacity()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn make_address(byte: u8) -> XorName {
        let mut addr = [0u8; 32];
        addr[0] = byte;
        addr
    }

    #[test]
    fn test_new_cache_default_capacity() {
        let cache = ChunkCache::default();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_zero_capacity_falls_back_to_default() {
        let cache = ChunkCache::new(0);
        let addr = make_address(1);
        cache.put(addr, Bytes::from_static(b"hello"));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_put_and_get() {
        let cache = ChunkCache::new(10);
        let addr = make_address(1);
        let data = Bytes::from_static(b"hello world");

        cache.put(addr, data.clone());
        let got = cache.get(&addr);
        assert_eq!(got, Some(data));
    }

    #[test]
    fn test_get_miss() {
        let cache = ChunkCache::new(10);
        let addr = make_address(1);
        assert_eq!(cache.get(&addr), None);
    }

    #[test]
    fn test_contains() {
        let cache = ChunkCache::new(10);
        let addr = make_address(1);
        assert!(!cache.contains(&addr));

        cache.put(addr, Bytes::from_static(b"data"));
        assert!(cache.contains(&addr));
    }

    #[test]
    fn test_lru_eviction() {
        let cache = ChunkCache::new(2);

        let addr1 = make_address(1);
        let addr2 = make_address(2);
        let addr3 = make_address(3);

        cache.put(addr1, Bytes::from_static(b"one"));
        cache.put(addr2, Bytes::from_static(b"two"));
        assert_eq!(cache.len(), 2);

        // Inserting a third should evict addr1 (least recently used)
        cache.put(addr3, Bytes::from_static(b"three"));
        assert_eq!(cache.len(), 2);
        assert!(!cache.contains(&addr1));
        assert!(cache.contains(&addr2));
        assert!(cache.contains(&addr3));
    }

    #[test]
    fn test_lru_access_refreshes() {
        let cache = ChunkCache::new(2);

        let addr1 = make_address(1);
        let addr2 = make_address(2);
        let addr3 = make_address(3);

        cache.put(addr1, Bytes::from_static(b"one"));
        cache.put(addr2, Bytes::from_static(b"two"));

        // Access addr1 to refresh it
        let _ = cache.get(&addr1);

        // Now inserting addr3 should evict addr2 (least recently used)
        cache.put(addr3, Bytes::from_static(b"three"));
        assert!(cache.contains(&addr1));
        assert!(!cache.contains(&addr2));
        assert!(cache.contains(&addr3));
    }

    #[test]
    fn test_clear() {
        let cache = ChunkCache::new(10);
        cache.put(make_address(1), Bytes::from_static(b"one"));
        cache.put(make_address(2), Bytes::from_static(b"two"));
        assert_eq!(cache.len(), 2);

        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_overwrite_same_key() {
        let cache = ChunkCache::new(10);
        let addr = make_address(1);

        cache.put(addr, Bytes::from_static(b"first"));
        cache.put(addr, Bytes::from_static(b"second"));

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&addr), Some(Bytes::from_static(b"second")));
    }
}
