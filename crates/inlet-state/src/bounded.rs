//! A map with a size cap that evicts the least recently used entry.

use std::collections::BTreeMap;

/// A map that holds at most `cap` entries. Inserting into a full map evicts
/// the entry that was inserted or touched longest ago. Lookups with
/// [`LruMap::get`] do not count as use; [`LruMap::touch`] does.
///
/// Iteration is in key order, so everything built from it is deterministic.
/// Equality compares the entries only, not their recency.
#[derive(Debug, Clone)]
pub(crate) struct LruMap<K: Ord + Copy, V> {
    entries: BTreeMap<K, (V, u64)>,
    order: BTreeMap<u64, K>,
    clock: u64,
    cap: usize,
}

impl<K: Ord + Copy, V: PartialEq> PartialEq for LruMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.cap == other.cap
            && self.entries.len() == other.entries.len()
            && self
                .entries
                .iter()
                .zip(other.entries.iter())
                .all(|((ka, (va, _)), (kb, (vb, _)))| ka == kb && va == vb)
    }
}

impl<K: Ord + Copy, V> LruMap<K, V> {
    pub(crate) fn new(cap: usize) -> Self {
        LruMap {
            entries: BTreeMap::new(),
            order: BTreeMap::new(),
            clock: 0,
            cap: cap.max(1),
        }
    }

    fn next_stamp(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Inserts or replaces `key` and marks it as most recently used. Returns
    /// the key that was evicted to make room, if any.
    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<K> {
        let stamp = self.next_stamp();
        if let Some((_, old)) = self.entries.insert(key, (value, stamp)) {
            self.order.remove(&old);
            self.order.insert(stamp, key);
            return None;
        }
        self.order.insert(stamp, key);
        if self.entries.len() > self.cap {
            if let Some((_, oldest)) = self.order.pop_first() {
                self.entries.remove(&oldest);
                return Some(oldest);
            }
        }
        None
    }

    /// Marks `key` as most recently used.
    pub(crate) fn touch(&mut self, key: &K) {
        let stamp = self.next_stamp();
        if let Some((_, s)) = self.entries.get_mut(key) {
            let old = std::mem::replace(s, stamp);
            self.order.remove(&old);
            self.order.insert(stamp, *key);
        }
    }

    /// The least recently used key whose value passes `pred`.
    pub(crate) fn oldest_where(&self, mut pred: impl FnMut(&V) -> bool) -> Option<K> {
        self.order
            .values()
            .find(|k| self.entries.get(*k).is_some_and(|(v, _)| pred(v)))
            .copied()
    }

    /// Removes and returns the least recently used entry.
    pub(crate) fn pop_oldest(&mut self) -> Option<(K, V)> {
        let (_, key) = self.order.pop_first()?;
        let (v, _) = self.entries.remove(&key)?;
        Some((key, v))
    }

    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|(v, _)| v)
    }

    pub(crate) fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.entries.get_mut(key).map(|(v, _)| v)
    }

    #[cfg(test)]
    pub(crate) fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<V> {
        let (v, stamp) = self.entries.remove(key)?;
        self.order.remove(&stamp);
        Some(v)
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        let order = &mut self.order;
        self.entries.retain(|k, (v, stamp)| {
            let kept = keep(k, v);
            if !kept {
                order.remove(stamp);
            }
            kept
        });
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|(k, (v, _))| (k, v))
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.entries.values_mut().map(|(v, _)| v)
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used() {
        let mut m = LruMap::new(2);
        assert_eq!(m.insert(1, "a"), None);
        assert_eq!(m.insert(2, "b"), None);
        m.touch(&1);
        assert_eq!(m.insert(3, "c"), Some(2));
        assert!(m.contains_key(&1) && m.contains_key(&3));
        assert_eq!(m.insert(3, "d"), None);
        assert_eq!(m.get(&3), Some(&"d"));
        assert_eq!(m.remove(&1), Some("a"));
        m.insert(4, "e");
        assert_eq!(m.pop_oldest(), Some((3, "d")));
        m.retain(|_, v| *v != "e");
        assert_eq!(m.len(), 0);
        assert_eq!(m.order.len(), 0);
    }
}
