use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelAvailability {
    Available,
    Missing,
    Unknown,
}

struct CachedModels {
    fetched_at: Instant,
    model_ids: HashSet<String>,
}

pub struct ModelAvailabilityCache {
    ttl: Duration,
    entries: HashMap<u64, CachedModels>,
}

impl ModelAvailabilityCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: HashMap::new(),
        }
    }

    pub fn lookup(&self, credential_id: u64, model: &str, now: Instant) -> Option<bool> {
        let entry = self.entries.get(&credential_id)?;
        if now.saturating_duration_since(entry.fetched_at) > self.ttl {
            return None;
        }
        Some(entry.model_ids.contains(model))
    }

    pub fn availability(&self, credential_id: u64, model: &str, now: Instant) -> ModelAvailability {
        match self.lookup(credential_id, model, now) {
            Some(true) => ModelAvailability::Available,
            Some(false) => ModelAvailability::Missing,
            None => ModelAvailability::Unknown,
        }
    }

    pub fn insert(
        &mut self,
        credential_id: u64,
        model_ids: impl IntoIterator<Item = String>,
        now: Instant,
    ) {
        self.entries.insert(
            credential_id,
            CachedModels {
                fetched_at: now,
                model_ids: model_ids.into_iter().collect(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn cache_is_isolated_by_credential() {
        let mut cache = ModelAvailabilityCache::new(Duration::from_secs(300));
        let now = Instant::now();
        cache.insert(1, ["claude-opus-4.8".to_string()], now);
        cache.insert(2, ["claude-sonnet-4.5".to_string()], now);

        assert_eq!(cache.lookup(1, "claude-opus-4.8", now), Some(true));
        assert_eq!(cache.lookup(2, "claude-opus-4.8", now), Some(false));
    }

    #[test]
    fn expired_entry_returns_unknown() {
        let mut cache = ModelAvailabilityCache::new(Duration::from_secs(60));
        let now = Instant::now();
        cache.insert(1, ["claude-opus-4.8".to_string()], now);
        assert_eq!(
            cache.lookup(1, "claude-opus-4.8", now + Duration::from_secs(61)),
            None
        );
    }

    #[test]
    fn cached_lookup_maps_to_public_availability() {
        let mut cache = ModelAvailabilityCache::new(Duration::from_secs(300));
        let now = Instant::now();
        cache.insert(7, ["claude-opus-4.8".to_string()], now);

        assert_eq!(
            cache.availability(7, "claude-opus-4.8", now),
            ModelAvailability::Available
        );
        assert_eq!(
            cache.availability(7, "claude-sonnet-4.5", now),
            ModelAvailability::Missing
        );
        assert_eq!(
            cache.availability(8, "claude-opus-4.8", now),
            ModelAvailability::Unknown
        );
    }
}
