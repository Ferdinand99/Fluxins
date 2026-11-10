//! An LRU cache counted in bytes, not entries.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

struct Entry<V> {
    value: V,
    size: u64,
    tick: u64,
}

/// Least-recently-used cache with a byte budget. An entry larger than the whole
/// budget is not stored. Not thread-safe; callers wrap it in a mutex.
pub(crate) struct ByteLru<K, V> {
    cap: u64,
    used: u64,
    tick: u64,
    map: HashMap<K, Entry<V>>,
    order: BTreeMap<u64, K>,
}

impl<K: Hash + Eq + Clone, V: Clone> ByteLru<K, V> {
    pub fn new(cap: u64) -> Self {
        Self {
            cap,
            used: 0,
            tick: 0,
            map: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn used(&self) -> u64 {
        self.used
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Returns a clone and marks the entry as most recently used.
    pub fn get(&mut self, key: &K) -> Option<V> {
        let tick = self.next_tick();
        let entry = self.map.get_mut(key)?;
        self.order.remove(&entry.tick);
        entry.tick = tick;
        self.order.insert(tick, key.clone());
        Some(entry.value.clone())
    }

    #[cfg(test)]
    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Stores a value and evicts the oldest entries until the budget holds.
    /// Returns false when the value is larger than the budget.
    pub fn insert(&mut self, key: K, value: V, size: u64) -> bool {
        self.remove(&key);
        if size > self.cap {
            return false;
        }
        while self.used + size > self.cap {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some(entry) = self.map.remove(&oldest) {
                self.used -= entry.size;
            }
        }
        let tick = self.next_tick();
        self.used += size;
        self.order.insert(tick, key.clone());
        self.map.insert(key, Entry { value, size, tick });
        true
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let entry = self.map.remove(key)?;
        self.order.remove(&entry.tick);
        self.used -= entry.size;
        Some(entry.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_the_least_recently_used_first() {
        let mut lru = ByteLru::new(100);
        lru.insert("a", 1, 40);
        lru.insert("b", 2, 40);
        assert_eq!(lru.get(&"a"), Some(1)); // a is now newer than b
        lru.insert("c", 3, 40);
        assert!(lru.contains(&"a"));
        assert!(!lru.contains(&"b"));
        assert!(lru.contains(&"c"));
        assert_eq!(lru.used(), 80);
    }

    #[test]
    fn counts_bytes_not_entries() {
        let mut lru = ByteLru::new(100);
        for i in 0..10 {
            lru.insert(i, i, 10);
        }
        assert_eq!(lru.len(), 10);
        lru.insert(99, 99, 55);
        assert_eq!(lru.used(), 95);
        assert_eq!(lru.len(), 5); // four small entries and the big one
        assert!(!lru.contains(&0) && lru.contains(&9));
    }

    #[test]
    fn oversized_entries_are_not_stored() {
        let mut lru = ByteLru::new(100);
        lru.insert("a", 1, 10);
        assert!(!lru.insert("big", 2, 101));
        assert!(!lru.contains(&"big"));
        assert_eq!(lru.used(), 10);
    }

    #[test]
    fn replacing_a_key_does_not_leak_bytes() {
        let mut lru = ByteLru::new(100);
        lru.insert("a", 1, 30);
        lru.insert("a", 2, 50);
        assert_eq!(lru.used(), 50);
        assert_eq!(lru.get(&"a"), Some(2));
    }
}
