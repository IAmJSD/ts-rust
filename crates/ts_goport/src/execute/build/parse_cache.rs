//! Go: execute/build/parseCache.go.
//!
//! PORT: Go guards each entry with a mutex so parallel parses of one key
//! run once. The build orchestrator runs on one thread (see
//! build_task.rs), so the entries are a plain map behind a `RefCell`. The
//! borrow is not held while `parse` runs, so `parse` can use the cache.
//!
//! PORT: Go `V` is a pointer, and its zero value is nil. Here the cached
//! value is `Option<V>`, and `None` is the Go zero value.
//!
//! PORT: a parallel build (builders.rs) shares one cache of `.d.ts` and
//! `.json` parses between its threads, with Go's entry locks
//! (`SyncParseCache`).

use crate::frontend::prelude::*;
use std::hash::Hash;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

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

// Go: build/parseCache.go:14 parseCache, with the entry mutex.
// PORT: the cache of a parallel `tsc -b`, whose threads share it
// (host.rs `SharedSourceFiles`). Go locks a new entry while its first
// parse runs, and a thread that finds an entry locks it too, so it waits
// for that parse. Here an entry is `None` while a thread parses, and the
// others wait on its condvar.
pub struct SyncParseCache<K, V> {
    entries: Mutex<FxHashMap<K, Arc<SyncEntry<V>>>>,
}

// Go: build/parseCache.go:9 parseCacheEntry
struct SyncEntry<V> {
    /// None while a thread parses (Go: the entry is locked), then the
    /// value (Go `nil` is `Some(None)`).
    value: Mutex<Option<Option<V>>>,
    parsed: Condvar,
}

impl<K, V> Default for SyncParseCache<K, V> {
    fn default() -> Self {
        SyncParseCache {
            entries: Mutex::new(FxHashMap::default()),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Stores the value of an entry that a thread parses and wakes the threads
/// that wait for it. A parse that panics stores `nil`, as Go's deferred
/// unlock leaves the zero value.
struct Parsed<'a, V>(&'a SyncEntry<V>, Option<Option<V>>);

impl<V> Drop for Parsed<'_, V> {
    fn drop(&mut self) {
        *lock(&self.0.value) = Some(self.1.take().flatten());
        self.0.parsed.notify_all();
    }
}

impl<K: Clone + Eq + Hash, V: Clone> SyncParseCache<K, V> {
    // Go: build/parseCache.go:18 (*parseCache).loadOrStore
    pub fn load_or_store(
        &self,
        key: K,
        parse: impl FnOnce(&K) -> Option<V>,
        allow_zero: bool,
    ) -> Option<V> {
        let (entry, loaded) = match lock(&self.entries).entry(key.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => (entry.get().clone(), true),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let new_entry = Arc::new(SyncEntry {
                    value: Mutex::new(None),
                    parsed: Condvar::new(),
                });
                entry.insert(new_entry.clone());
                (new_entry, false)
            }
        };
        if loaded {
            let mut value = lock(&entry.value);
            loop {
                match &*value {
                    None => {
                        value = entry
                            .parsed
                            .wait(value)
                            .unwrap_or_else(PoisonError::into_inner);
                    }
                    Some(stored) => {
                        if allow_zero || stored.is_some() {
                            return stored.clone();
                        }
                        break;
                    }
                }
            }
            // Go `newEntry = entry`: this thread parses again, and the
            // others wait for it.
            *value = None;
        }
        let mut parsed = Parsed(&entry, None);
        let value = parse(&key);
        parsed.1 = Some(value.clone());
        value
    }
}
