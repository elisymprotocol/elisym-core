use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

/// A bounded deduplication set that evicts the oldest entries when full.
///
/// Used in subscription handlers to prevent duplicate event delivery
/// without unbounded memory growth in long-running processes.
pub(crate) struct BoundedDedup<T> {
    set: HashSet<T>,
    order: VecDeque<T>,
    capacity: usize,
}

impl<T: Hash + Eq + Clone> BoundedDedup<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            set: HashSet::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Returns `true` if the item is new (not seen before).
    /// Returns `false` if it's a duplicate.
    pub fn insert(&mut self, item: T) -> bool {
        if self.set.contains(&item) {
            return false;
        }
        if self.order.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.set.remove(&evicted);
            }
        }
        self.set.insert(item.clone());
        self.order.push_back(item);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedup_basic() {
        let mut d = BoundedDedup::new(3);
        assert!(d.insert(1));
        assert!(d.insert(2));
        assert!(!d.insert(1)); // duplicate
        assert!(d.insert(3));
        assert_eq!(d.set.len(), 3);
    }

    #[test]
    fn test_dedup_eviction() {
        let mut d = BoundedDedup::new(3);
        assert!(d.insert(1));
        assert!(d.insert(2));
        assert!(d.insert(3));
        // Full — inserting 4 evicts 1 (oldest)
        assert!(d.insert(4));
        assert_eq!(d.set.len(), 3);
        assert!(d.insert(1)); // 1 was evicted, so it's "new" again
        // Now set is {3, 4, 1} — 2 was evicted when 1 was inserted
        assert!(d.insert(2)); // 2 was evicted too
        assert!(!d.insert(4)); // 4 is still in the set
    }
}
