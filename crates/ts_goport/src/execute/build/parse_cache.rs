//! Go: execute/build/parseCache.go.
//!
//! PORT: Go guards each entry with a mutex so parallel parses of one key
//! run once. The build orchestrator runs on one thread (see
//! build_task.rs), so the entries are a plain map behind a `RefCell`. The
//! borrow is not held while `parse` runs, so `parse` can use the cache.
//!
//! PORT: Go `V` is a pointer, and its zero value is nil. Here the cached
//! value is `Option<V>`, and `None` is the Go zero value.

use crate::frontend::prelude::*;
use std::hash::Hash;

// Go: build/parseCache.go:9 parseCacheEntry
// PORT: the entry is the cached `Option<V>` itself (the mutex is dropped).

// Go: build/parseCache.go:14 parseCache
pub struct ParseCache<K, V> {
    entries: RefCell<FxHashMap<K, Option<V>>>,
}

impl<K, V> Default for ParseCache<K, V> {
    fn default() -> Self {
        ParseCache {
            entries: RefCell::new(FxHashMap::default()),
        }
    }
}

impl<K: Clone + Eq + Hash, V: Clone> ParseCache<K, V> {
    // Go: build/parseCache.go:18 (*parseCache).loadOrStore
    pub fn load_or_store(
        &self,
        key: K,
        parse: impl FnOnce(&K) -> Option<V>,
        allow_zero: bool,
    ) -> Option<V> {
        let existing = self.entries.borrow().get(&key).cloned();
        match existing {
            Some(value) => {
                if allow_zero || value.is_some() {
                    return value;
                }
            }
            None => {
                // PORT: Go stores the new (locked) entry before it parses.
                self.entries.borrow_mut().insert(key.clone(), None);
            }
        }
        let value = parse(&key);
        self.entries.borrow_mut().insert(key, value.clone());
        value
    }

    /// Calls `f` with each key that has a stored value, and the value.
    // PORT: not in Go (see `CompilerHost::cached_source_file_refs`).
    pub fn for_each_stored(&self, mut f: impl FnMut(&K, &V)) {
        for (key, value) in self.entries.borrow().iter() {
            if let Some(value) = value {
                f(key, value);
            }
        }
    }

    /// Calls `f` with each key and its entry: `None` for a parse that gave
    /// no value.
    // PORT: not in Go (perf, builders.rs).
    pub fn for_each_entry(&self, mut f: impl FnMut(&K, Option<&V>)) {
        for (key, value) in self.entries.borrow().iter() {
            f(key, value.as_ref());
        }
    }

    // Go: build/parseCache.go:34 (*parseCache).store
    pub fn store(&self, key: K, value: Option<V>) {
        self.entries.borrow_mut().insert(key, value);
    }

    // Go: build/parseCache.go:38 (*parseCache).delete
    pub fn delete(&self, key: &K) {
        self.entries.borrow_mut().remove(key);
    }

    // Go: build/parseCache.go:42 (*parseCache).reset
    pub fn reset(&self) {
        self.entries.borrow_mut().clear();
    }
}
