use dashmap::DashMap;
use std::collections::VecDeque;
use std::sync::Mutex;

/// Dedup state: InFlight means extraction in progress, Done means fully processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupState {
    InFlight,
    Done,
}

/// Thread-safe, atomic dedup cache with bounded capacity.
/// Uses DashMap for lock-free concurrent access and a Mutex<VecDeque> for eviction order.
pub struct AtomicDedupCache {
    map: DashMap<String, DedupState>,
    order: Mutex<VecDeque<String>>,
    capacity: usize,
}

impl AtomicDedupCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            map: DashMap::with_capacity(capacity),
            order: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    /// Try to admit a message ID. Returns `true` if this caller won the race
    /// (inserted as InFlight). Returns `false` if already present.
    pub fn try_admit(&self, id: &str) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.map.entry(id.to_string()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(v) => {
                v.insert(DedupState::InFlight);
                let mut order = self.order.lock().unwrap();
                order.push_back(id.to_string());
                // Evict oldest if over capacity
                while order.len() > self.capacity {
                    if let Some(old) = order.pop_front() {
                        self.map.remove(&old);
                    }
                }
                true
            }
        }
    }

    /// Mark a message as fully processed.
    pub fn mark_done(&self, id: &str) {
        if let Some(mut entry) = self.map.get_mut(id) {
            *entry = DedupState::Done;
        }
    }

    /// Remove a message from dedup (allows retry on transient failure).
    pub fn remove(&self, id: &str) {
        self.map.remove(id);
    }

    /// Check if an ID is present (any state).
    pub fn contains(&self, id: &str) -> bool {
        self.map.contains_key(id)
    }

    /// Current number of entries.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_admit_returns_true_first_time() {
        let cache = AtomicDedupCache::new(10);
        assert!(cache.try_admit("msg1"));
        assert!(cache.contains("msg1"));
    }

    #[test]
    fn test_admit_returns_false_for_duplicate() {
        let cache = AtomicDedupCache::new(10);
        assert!(cache.try_admit("msg1"));
        assert!(!cache.try_admit("msg1"));
    }

    #[test]
    fn test_mark_done() {
        let cache = AtomicDedupCache::new(10);
        cache.try_admit("msg1");
        cache.mark_done("msg1");
        assert!(!cache.try_admit("msg1"));
        assert!(cache.contains("msg1"));
    }

    #[test]
    fn test_remove_allows_retry() {
        let cache = AtomicDedupCache::new(10);
        cache.try_admit("msg1");
        cache.remove("msg1");
        assert!(cache.try_admit("msg1"));
    }

    #[test]
    fn test_capacity_eviction() {
        let cache = AtomicDedupCache::new(3);
        cache.try_admit("a");
        cache.try_admit("b");
        cache.try_admit("c");
        cache.try_admit("d");
        assert!(!cache.contains("a"));
        assert!(cache.contains("b"));
        assert!(cache.contains("d"));
    }

    #[test]
    fn test_concurrent_admit_only_one_wins() {
        let cache = Arc::new(AtomicDedupCache::new(100));
        let mut handles = vec![];
        let wins = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        for _ in 0..100 {
            let c = cache.clone();
            let w = wins.clone();
            handles.push(std::thread::spawn(move || {
                if c.try_admit("race_id") {
                    w.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(wins.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
