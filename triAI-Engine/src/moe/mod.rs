use crate::supervisor::expert_tracker::ExpertEvent;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExpertKey {
    pub layer_id: u32,
    pub expert_id: u32,
}

#[derive(Debug, Clone)]
pub struct ExpertInfo {
    pub key: ExpertKey,
    pub access_count: u64,
    pub last_accessed: Instant,
    pub is_hot: bool,
    pub is_pinned: bool,
}

#[derive(Debug)]
pub struct ExpertResidency {
    experts: HashMap<ExpertKey, ExpertInfo>,
    hot_set: HashSet<ExpertKey>,
    cold_set: HashSet<ExpertKey>,
    access_threshold: u64,
    total_events: u64,
}
impl Default for ExpertResidency {
    fn default() -> Self {
        Self {
            experts: HashMap::new(),
            hot_set: HashSet::new(),
            cold_set: HashSet::new(),
            access_threshold: 5,
            total_events: 0,
        }
    }
}
impl ExpertResidency {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_threshold(threshold: u64) -> Self {
        Self {
            access_threshold: threshold,
            ..Self::default()
        }
    }
    pub fn record_event(&mut self, event: &ExpertEvent) {
        self.total_events += 1;
        for &expert_id in &event.expert_ids {
            let key = ExpertKey {
                layer_id: event.layer_id,
                expert_id,
            };
            let info = self.experts.entry(key).or_insert_with(|| ExpertInfo {
                key: key.clone(),
                access_count: 0,
                last_accessed: Instant::now(),
                is_hot: false,
                is_pinned: false,
            });
            info.access_count += 1;
            info.last_accessed = Instant::now();
            self.reclassify(&key);
        }
    }
    pub fn record_events(&mut self, events: &[ExpertEvent]) {
        for event in events {
            self.record_event(event);
        }
    }
    fn reclassify(&mut self, key: &ExpertKey) {
        let count = self.experts.get(key).map(|e| e.access_count).unwrap_or(0);
        let is_hot = count >= self.access_threshold;
        if is_hot {
            self.hot_set.insert(key.clone());
            self.cold_set.remove(key);
            if let Some(info) = self.experts.get_mut(key) {
                info.is_hot = true;
            }
        } else {
            self.cold_set.insert(key.clone());
            self.hot_set.remove(key);
            if let Some(info) = self.experts.get_mut(key) {
                info.is_hot = false;
            }
        }
    }
    pub fn hot_experts(&self) -> Vec<ExpertKey> {
        self.hot_set.iter().cloned().collect()
    }
    pub fn cold_experts(&self) -> Vec<ExpertKey> {
        self.cold_set.iter().cloned().collect()
    }
    pub fn is_hot(&self, key: &ExpertKey) -> bool {
        self.hot_set.contains(key)
    }
    pub fn hit_rate(&self) -> f64 {
        if self.total_events == 0 {
            return 0.0;
        }
        let hot_events = self.experts.values().filter(|e| e.is_hot).map(|e| e.access_count).sum::<u64>();
        hot_events as f64 / self.total_events as f64
    }
    pub fn pin_expert(&mut self, key: &ExpertKey) {
        if let Some(info) = self.experts.get_mut(key) {
            info.is_pinned = true;
        }
    }
    pub fn unpin_expert(&mut self, key: &ExpertKey) {
        if let Some(info) = self.experts.get_mut(key) {
            info.is_pinned = false;
        }
    }
    pub fn pinned_experts(&self) -> Vec<ExpertKey> {
        self.experts.values().filter(|e| e.is_pinned).map(|e| e.key.clone()).collect()
    }
    pub fn stats(&self) -> ExpertStats {
        ExpertStats {
            total_experts: self.experts.len(),
            hot_count: self.hot_set.len(),
            cold_count: self.cold_set.len(),
            pinned_count: self.pinned_experts().len(),
            total_events: self.total_events,
            hit_rate: self.hit_rate(),
        }
    }
}
#[derive(Debug, Clone)]
pub struct ExpertStats {
    pub total_experts: usize,
    pub hot_count: usize,
    pub cold_count: usize,
    pub pinned_count: usize,
    pub total_events: u64,
    pub hit_rate: f64,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn records_and_classifies_experts() {
        let mut residency = ExpertResidency::with_threshold(3);
        let event = ExpertEvent {
            layer_id: 0,
            expert_ids: vec![1, 2],
            timestamp_ms: 1000,
        };
        residency.record_event(&event);
        let key1 = ExpertKey { layer_id: 0, expert_id: 1 };
        assert!(!residency.is_hot(&key1));
        for _ in 0..3 {
            residency.record_event(&event);
        }
        assert!(residency.is_hot(&key1));
    }
    #[test]
    fn hit_rate_calculation() {
        let mut residency = ExpertResidency::with_threshold(2);
        let event = ExpertEvent {
            layer_id: 0,
            expert_ids: vec![1],
            timestamp_ms: 1000,
        };
        for _ in 0..5 {
            residency.record_event(&event);
        }
        let stats = residency.stats();
        assert_eq!(stats.total_experts, 1);
        assert_eq!(stats.hot_count, 1);
        assert!(stats.hit_rate > 0.8);
    }
    #[test]
    fn pinning_works() {
        let mut residency = ExpertResidency::new();
        let key = ExpertKey { layer_id: 0, expert_id: 1 };
        let event = ExpertEvent {
            layer_id: 0,
            expert_ids: vec![1],
            timestamp_ms: 1000,
        };
        residency.record_event(&event);
        residency.pin_expert(&key);
        assert!(residency.pinned_experts().contains(&key));
        residency.unpin_expert(&key);
        assert!(!residency.pinned_experts().contains(&key));
    }
}
