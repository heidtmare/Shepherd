//! Short-TTL cache of facet counts keyed by the match-defining part of a plan.
//!
//! Facet counts are the most expensive part of a broad query and are identical across the pages of
//! one result set and across repeated loads of the same worklist. Per process; staleness <= TTL.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::model::FacetCount;

const MAX_ENTRIES: usize = 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct FacetCounts {
    pub counts: BTreeMap<String, Vec<FacetCount>>,
    pub exact: bool,
}

pub struct FacetCache {
    ttl: Duration,
    entries: Mutex<HashMap<String, (Instant, FacetCounts)>>,
}

impl FacetCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.ttl.is_zero()
    }

    pub fn get(&self, key: &str) -> Option<FacetCounts> {
        let entries = self.entries.lock().expect("facet cache lock");
        entries
            .get(key)
            .filter(|(at, _)| at.elapsed() < self.ttl)
            .map(|(_, v)| v.clone())
    }

    pub fn put(&self, key: String, value: FacetCounts) {
        let mut entries = self.entries.lock().expect("facet cache lock");
        if entries.len() >= MAX_ENTRIES {
            entries.retain(|_, (at, _)| at.elapsed() < self.ttl);
            if entries.len() >= MAX_ENTRIES {
                entries.clear();
            }
        }
        entries.insert(key, (Instant::now(), value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts() -> FacetCounts {
        FacetCounts {
            counts: BTreeMap::from([(
                "type".into(),
                vec![FacetCount {
                    value: "a".into(),
                    count: 1,
                }],
            )]),
            exact: true,
        }
    }

    #[test]
    fn hit_expiry_and_bound() {
        let c = FacetCache::new(Duration::from_millis(50));
        c.put("k".into(), counts());
        assert_eq!(c.get("k"), Some(counts()));
        assert_eq!(c.get("other"), None);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(c.get("k"), None);
        for i in 0..MAX_ENTRIES + 10 {
            c.put(i.to_string(), counts());
        }
        assert!(c.entries.lock().unwrap().len() <= MAX_ENTRIES);
        assert!(!FacetCache::new(Duration::ZERO).enabled());
    }
}
