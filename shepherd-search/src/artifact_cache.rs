//! Parsed artifact bodies keyed by artifact id.
//!
//! Artifacts are immutable, so entries never go stale. A document is re-indexed on every event
//! that touches it, and each re-index reads the latest artifact of every schema; the cache turns
//! the unchanged ones into memory hits. Bounded by the serialized size of the cached bodies;
//! evicts oldest-inserted first. Per process.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use serde_json::Value;
use uuid::Uuid;

pub struct ArtifactCache {
    max_bytes: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    bytes: usize,
    entries: HashMap<Uuid, (usize, Arc<Value>)>,
    order: VecDeque<Uuid>,
}

impl ArtifactCache {
    /// `max_bytes = 0` disables the cache.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn get(&self, id: Uuid) -> Option<Arc<Value>> {
        let inner = self.inner.lock().expect("artifact cache lock");
        inner.entries.get(&id).map(|(_, v)| v.clone())
    }

    /// `size` is the serialized length. Bodies larger than 1/8 of the budget are not cached.
    pub fn put(&self, id: Uuid, size: usize, body: Arc<Value>) {
        if self.max_bytes == 0 || size > self.max_bytes / 8 {
            return;
        }
        let mut inner = self.inner.lock().expect("artifact cache lock");
        if inner.entries.contains_key(&id) {
            return;
        }
        while inner.bytes + size > self.max_bytes {
            let Some(old) = inner.order.pop_front() else {
                break;
            };
            if let Some((n, _)) = inner.entries.remove(&old) {
                inner.bytes -= n;
            }
        }
        inner.bytes += size;
        inner.entries.insert(id, (size, body));
        inner.order.push_back(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_by_bytes() {
        let c = ArtifactCache::new(800);
        let ids: Vec<Uuid> = (0..10).map(|_| Uuid::now_v7()).collect();
        for id in &ids {
            c.put(*id, 100, Arc::new(Value::Null));
        }
        assert!(c.get(ids[0]).is_none() && c.get(ids[1]).is_none());
        assert!(ids[2..].iter().all(|id| c.get(*id).is_some()));
        assert_eq!(c.inner.lock().unwrap().bytes, 800);

        let big = Uuid::now_v7();
        c.put(big, 101, Arc::new(Value::Null));
        assert!(c.get(big).is_none(), "over 1/8 of budget");

        let off = ArtifactCache::new(0);
        off.put(big, 0, Arc::new(Value::Null));
        assert!(off.get(big).is_none());
    }
}
