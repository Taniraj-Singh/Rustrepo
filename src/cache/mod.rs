//! A capacity-bounded LRU cache, and the block cache built on it.
//!
//! Decoding a data block (checksum verification plus decompression) is not free,
//! so decoded blocks are cached and shared across reads. [`LruCache`] is a
//! generic least-recently-used cache bounded by a *charge* (here, bytes), and
//! [`BlockCache`] wraps it behind a mutex with the engine's `(table, offset)`
//! key type and reference-counted block payloads.
//!
//! The LRU order is maintained with a monotonic access clock and an ordered map:
//! every access stamps the entry with the next tick, and eviction removes the
//! entry with the smallest tick. This is `O(log n)` per operation and needs no
//! `unsafe` linked-list bookkeeping.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::sync::{Arc, Mutex};

/// A least-recently-used cache mapping `K` to `V`, bounded by total charge.
///
/// Each entry declares a `charge` (its cost against the capacity). When
/// inserting would exceed [`LruCache::capacity`], the least recently used
/// entries are evicted until it fits. An entry whose own charge exceeds the
/// capacity is simply not stored.
#[derive(Debug)]
pub struct LruCache<K, V> {
    capacity: usize,
    charge: usize,
    tick: u64,
    // key -> (value, charge, last-access tick)
    map: HashMap<K, (V, usize, u64)>,
    // tick -> key, ordered so the front is the least recently used
    order: BTreeMap<u64, K>,
}

impl<K: Eq + Hash + Clone, V: Clone> LruCache<K, V> {
    /// Creates a cache holding up to `capacity` units of charge.
    pub fn new(capacity: usize) -> LruCache<K, V> {
        LruCache {
            capacity,
            charge: 0,
            tick: 0,
            map: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    /// Current number of cached entries.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Total charge currently held.
    pub fn charge(&self) -> usize {
        self.charge
    }

    /// The configured capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Looks up `key`, marking it most-recently-used on a hit.
    pub fn get(&mut self, key: &K) -> Option<V> {
        // Fetch the old tick and value without holding a borrow across the
        // mutation of `order`.
        let (value, old_tick) = {
            let (v, _charge, tick) = self.map.get(key)?;
            (v.clone(), *tick)
        };
        let new_tick = self.next_tick();
        self.order.remove(&old_tick);
        self.order.insert(new_tick, key.clone());
        if let Some(slot) = self.map.get_mut(key) {
            slot.2 = new_tick;
        }
        Some(value)
    }

    /// Inserts or replaces `key`, evicting as needed to stay within capacity.
    pub fn insert(&mut self, key: K, value: V, charge: usize) {
        // Replace any existing entry first, reclaiming its charge and order slot.
        if let Some((_old_v, old_charge, old_tick)) = self.map.remove(&key) {
            self.order.remove(&old_tick);
            self.charge -= old_charge;
        }

        // An item larger than the whole cache is never stored.
        if charge > self.capacity {
            return;
        }

        let tick = self.next_tick();
        self.map.insert(key.clone(), (value, charge, tick));
        self.order.insert(tick, key);
        self.charge += charge;

        self.evict_to_fit();
    }

    /// Removes `key` if present, returning its value.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let (value, charge, tick) = self.map.remove(key)?;
        self.order.remove(&tick);
        self.charge -= charge;
        Some(value)
    }

    /// Drops all entries.
    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.charge = 0;
    }

    fn evict_to_fit(&mut self) {
        while self.charge > self.capacity {
            // The lowest tick is the least recently used entry.
            let Some((&tick, _key)) = self.order.iter().next() else {
                break;
            };
            let key = self
                .order
                .remove(&tick)
                .expect("tick just observed in order map");
            if let Some((_v, charge, _t)) = self.map.remove(&key) {
                self.charge -= charge;
            }
        }
    }
}

/// The key identifying a cached block: `(table id, block offset in file)`.
pub type BlockKey = (u64, u64);

/// A thread-safe cache of decoded data blocks shared across a database.
///
/// Payloads are `Arc<[u8]>` so a cache hit hands out a cheap reference-counted
/// clone rather than copying the block. A capacity of zero disables caching.
#[derive(Debug)]
pub struct BlockCache {
    inner: Mutex<LruCache<BlockKey, Arc<[u8]>>>,
    enabled: bool,
}

impl BlockCache {
    /// Creates a block cache of `capacity_bytes`. Zero disables it.
    pub fn new(capacity_bytes: usize) -> BlockCache {
        BlockCache {
            inner: Mutex::new(LruCache::new(capacity_bytes.max(1))),
            enabled: capacity_bytes > 0,
        }
    }

    /// Fetches a cached block, if present and caching is enabled.
    pub fn get(&self, table_id: u64, offset: u64) -> Option<Arc<[u8]>> {
        if !self.enabled {
            return None;
        }
        let mut guard = self.inner.lock().expect("block cache mutex poisoned");
        guard.get(&(table_id, offset))
    }

    /// Inserts a decoded block. The charge is the block's byte length.
    pub fn insert(&self, table_id: u64, offset: u64, block: Arc<[u8]>) {
        if !self.enabled {
            return;
        }
        let charge = block.len();
        let mut guard = self.inner.lock().expect("block cache mutex poisoned");
        guard.insert((table_id, offset), block, charge);
    }

    /// Number of cached blocks (for tests and metrics).
    pub fn len(&self) -> usize {
        self.inner.lock().expect("block cache mutex poisoned").len()
    }

    /// Whether the cache currently holds no blocks.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used() {
        let mut c: LruCache<i32, i32> = LruCache::new(3);
        c.insert(1, 10, 1);
        c.insert(2, 20, 1);
        c.insert(3, 30, 1);
        // Touch 1 so 2 becomes least recently used.
        assert_eq!(c.get(&1), Some(10));
        c.insert(4, 40, 1); // evicts key 2
        assert_eq!(c.get(&2), None);
        assert_eq!(c.get(&1), Some(10));
        assert_eq!(c.get(&3), Some(30));
        assert_eq!(c.get(&4), Some(40));
    }

    #[test]
    fn charge_bounds_capacity() {
        let mut c: LruCache<i32, Vec<u8>> = LruCache::new(100);
        c.insert(1, vec![0; 60], 60);
        c.insert(2, vec![0; 60], 60); // evicts key 1 (total would be 120)
        assert!(c.charge() <= 100);
        assert_eq!(c.get(&1), None);
        assert!(c.get(&2).is_some());
    }

    #[test]
    fn oversized_entry_not_stored() {
        let mut c: LruCache<i32, i32> = LruCache::new(10);
        c.insert(1, 1, 999);
        assert!(c.is_empty());
    }

    #[test]
    fn replace_updates_charge() {
        let mut c: LruCache<i32, i32> = LruCache::new(10);
        c.insert(1, 1, 5);
        c.insert(1, 2, 3);
        assert_eq!(c.len(), 1);
        assert_eq!(c.charge(), 3);
        assert_eq!(c.get(&1), Some(2));
    }

    #[test]
    fn block_cache_shares_arcs() {
        let bc = BlockCache::new(1024);
        let block: Arc<[u8]> = Arc::from(vec![1u8, 2, 3].into_boxed_slice());
        bc.insert(7, 128, block.clone());
        let got = bc.get(7, 128).unwrap();
        assert_eq!(&*got, &[1, 2, 3]);
        assert!(bc.get(7, 999).is_none());
    }

    #[test]
    fn disabled_cache_stores_nothing() {
        let bc = BlockCache::new(0);
        bc.insert(1, 0, Arc::from(vec![0u8; 8].into_boxed_slice()));
        assert!(bc.get(1, 0).is_none());
        assert!(bc.is_empty());
    }
}
