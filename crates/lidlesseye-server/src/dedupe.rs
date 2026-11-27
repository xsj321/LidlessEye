//! Replay protection for ingest batches.
//!
//! A batch can arrive twice. The agent's spool re-sends anything it did not get
//! a `2xx` for, and a reply lost on the way back looks exactly like a failure;
//! a proxy retry has the same shape. Counting those rows twice inflates every
//! total downstream, and the error is invisible because both deliveries look
//! successful. A batch that arrives again with the same id is acknowledged and
//! ignored instead.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;

/// How many batch ids are remembered. A five-minute window per batch means this
/// covers several days of ordinary ingestion, and the ids are tiny.
const CAPACITY: usize = 8192;

/// How long an id is remembered. Long enough to outlive any retry budget, short
/// enough that a recycled id from a restarted agent does not swallow a batch.
const TTL_NS: u64 = 30 * 60 * 1_000_000_000;

#[derive(Debug, Default)]
pub struct Dedupe {
    seen: Mutex<Seen>,
}

#[derive(Debug, Default)]
struct Seen {
    at: BTreeMap<String, u64>,
    order: VecDeque<String>,
}

impl Dedupe {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Has this batch id already been accepted?
    pub fn seen(&self, id: &str) -> bool {
        let now = lidlesseye_core::now_ns();
        let mut seen = self.seen.lock();
        Self::evict(&mut seen, now);
        seen.at
            .get(id)
            .is_some_and(|at| now.saturating_sub(*at) <= TTL_NS)
    }

    /// Record a batch id as accepted.
    ///
    /// This happens only after the batch is stored. Marking it before parsing
    /// would blackhole a batch that failed on a fixable error, and the agent
    /// could then never deliver it again under the same id.
    pub fn remember(&self, id: &str) {
        let now = lidlesseye_core::now_ns();
        let mut seen = self.seen.lock();
        Self::evict(&mut seen, now);
        if seen.at.contains_key(id) {
            return;
        }
        seen.at.insert(id.to_string(), now);
        seen.order.push_back(id.to_string());
    }

    fn evict(seen: &mut Seen, now: u64) {
        while let Some(front) = seen.order.front() {
            let expired = seen
                .at
                .get(front)
                .is_none_or(|at| now.saturating_sub(*at) > TTL_NS);
            if !expired && seen.order.len() <= CAPACITY {
                break;
            }
            let key = seen.order.pop_front().unwrap_or_default();
            seen.at.remove(&key);
        }
    }

    /// Number of remembered ids, for tests and diagnostics.
    pub fn len(&self) -> usize {
        self.seen.lock().at.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_id_is_a_replay_only_once() {
        let dedupe = Dedupe::default();
        assert!(!dedupe.seen("a"));
        dedupe.remember("a");
        assert!(dedupe.seen("a"));
        assert!(!dedupe.seen("b"));
    }

    #[test]
    fn the_oldest_ids_are_forgotten() {
        let dedupe = Dedupe::default();
        for index in 0..(CAPACITY + 10) {
            dedupe.remember(&format!("id-{index}"));
        }
        assert!(dedupe.len() <= CAPACITY);
        // The first one has been evicted, so it is a fresh batch again.
        assert!(!dedupe.seen("id-0"));
    }
}
