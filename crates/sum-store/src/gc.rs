//! Garbage collector for unassigned chunks.
//!
//! When nodes join or leave the network, the deterministic assignment
//! recomputes (because the node list modulus changes). A node that was
//! assigned to a chunk may no longer be assigned after a new node registers.
//! Without GC, that node holds the unassigned chunk indefinitely.
//!
//! The [`GarbageCollector`] runs after each MarketSync cycle. It marks
//! chunks that are no longer assigned, and deletes them once they have
//! been unassigned for longer than the configured grace period.
//!
//! Safety guarantees:
//! - Never deletes a chunk that is currently assigned.
//! - Respects a configurable grace period (default 1 hour) to avoid
//!   thrashing from transient node list changes.
//! - Pauses if the L1 has not been polled within 5 minutes (stale state).
//! - `unassigned_since` is in-memory only — on restart, the grace period
//!   resets, which is the conservative/safe behavior.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::error::Result;
use crate::store::ChunkStore;

/// Maximum age of the last L1 poll before GC is paused.
const MAX_L1_POLL_AGE: Duration = Duration::from_secs(300); // 5 minutes

// ── Public Types ─────────────────────────────────────────────────────────────

/// Garbage collector for unassigned chunks.
pub struct GarbageCollector {
    /// CID -> first time we noticed this CID was unassigned.
    unassigned_since: HashMap<String, Instant>,
    /// Grace period before deletion.
    grace_period: Duration,
}

/// Result of a GC run.
#[derive(Debug, Default)]
pub struct GcResult {
    /// Number of chunks deleted in this run.
    pub chunks_deleted: u32,
    /// Total bytes freed.
    pub bytes_freed: u64,
    /// Number of unassigned chunks retained (within grace period).
    pub chunks_retained: u32,
    /// Whether GC was skipped (e.g., stale L1 state).
    pub skipped: bool,
}

// ── Implementation ───────────────────────────────────────────────────────────

impl GarbageCollector {
    /// Create a new garbage collector with the given grace period.
    pub fn new(grace_period: Duration) -> Self {
        Self {
            unassigned_since: HashMap::new(),
            grace_period,
        }
    }

    /// Run a mark-and-sweep GC cycle.
    ///
    /// `assigned_cids` is the complete set of CIDs this node is currently
    /// assigned to across all funded files.
    ///
    /// `last_l1_poll` is when the L1 was last successfully polled. If this
    /// is more than 5 minutes ago, GC is skipped entirely to avoid
    /// deleting based on stale assignment state.
    pub fn mark_and_sweep(
        &mut self,
        store: &ChunkStore,
        assigned_cids: &HashSet<String>,
        last_l1_poll: Instant,
    ) -> Result<GcResult> {
        // Safety: don't GC based on stale L1 state
        if last_l1_poll.elapsed() > MAX_L1_POLL_AGE {
            warn!(
                elapsed_secs = last_l1_poll.elapsed().as_secs(),
                "gc skipped: last L1 poll too old (> 5 min)"
            );
            return Ok(GcResult { skipped: true, ..Default::default() });
        }

        let all_cids = store.list_all_cids()?;
        let now = Instant::now();
        let mut result = GcResult::default();

        // Remove from tracking any CIDs that are now assigned (re-assigned)
        self.unassigned_since.retain(|cid, _| !assigned_cids.contains(cid));

        for cid in &all_cids {
            if assigned_cids.contains(cid) {
                // Assigned — keep it, don't track
                continue;
            }

            // Mark as unassigned if not already tracked
            let first_seen = *self.unassigned_since
                .entry(cid.clone())
                .or_insert(now);

            let unassigned_duration = now.duration_since(first_seen);

            if unassigned_duration >= self.grace_period {
                // Past grace period — delete
                let size = store.get(cid).map(|d| d.len() as u64).unwrap_or(0);
                match store.delete(cid) {
                    Ok(true) => {
                        info!(
                            %cid,
                            unassigned_secs = unassigned_duration.as_secs(),
                            bytes = size,
                            "gc: deleted unassigned chunk"
                        );
                        result.chunks_deleted += 1;
                        result.bytes_freed += size;
                        self.unassigned_since.remove(cid);
                    }
                    Ok(false) => { /* already gone */ }
                    Err(e) => {
                        warn!(%cid, %e, "gc: failed to delete chunk");
                    }
                }
            } else {
                result.chunks_retained += 1;
                debug!(
                    %cid,
                    remaining_secs = (self.grace_period.saturating_sub(unassigned_duration)).as_secs(),
                    "gc: unassigned chunk within grace period"
                );
            }
        }

        if result.chunks_deleted > 0 {
            info!(
                deleted = result.chunks_deleted,
                bytes_freed = result.bytes_freed,
                retained = result.chunks_retained,
                "gc: sweep complete"
            );
        }

        Ok(result)
    }

    /// Number of chunks currently tracked as unassigned (within grace period).
    pub fn tracked_count(&self) -> usize {
        self.unassigned_since.len()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> (tempfile::TempDir, ChunkStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(dir.path().join("chunks")).unwrap();
        (dir, store)
    }

    #[test]
    fn gc_assigned_chunk_not_deleted() {
        let (_dir, store) = make_store();
        store.put("cid_a", b"chunk a data").unwrap();

        let mut assigned = HashSet::new();
        assigned.insert("cid_a".to_string());

        let mut gc = GarbageCollector::new(Duration::from_secs(0)); // 0 grace
        let result = gc.mark_and_sweep(&store, &assigned, Instant::now()).unwrap();

        assert_eq!(result.chunks_deleted, 0);
        assert!(store.has("cid_a"));
    }

    #[test]
    fn gc_unassigned_within_grace() {
        let (_dir, store) = make_store();
        store.put("cid_b", b"chunk b data").unwrap();

        let assigned: HashSet<String> = HashSet::new(); // cid_b not assigned

        let mut gc = GarbageCollector::new(Duration::from_secs(3600)); // 1 hour grace
        let result = gc.mark_and_sweep(&store, &assigned, Instant::now()).unwrap();

        assert_eq!(result.chunks_deleted, 0);
        assert_eq!(result.chunks_retained, 1);
        assert!(store.has("cid_b")); // still on disk
    }

    #[test]
    fn gc_unassigned_past_grace() {
        let (_dir, store) = make_store();
        store.put("cid_c", b"chunk c data").unwrap();

        let assigned: HashSet<String> = HashSet::new();

        let mut gc = GarbageCollector::new(Duration::from_secs(0)); // 0 grace = delete immediately
        let result = gc.mark_and_sweep(&store, &assigned, Instant::now()).unwrap();

        assert_eq!(result.chunks_deleted, 1);
        assert!(result.bytes_freed > 0);
        assert!(!store.has("cid_c")); // deleted
    }

    #[test]
    fn gc_reassigned_during_grace() {
        let (_dir, store) = make_store();
        store.put("cid_d", b"chunk d data").unwrap();

        let mut gc = GarbageCollector::new(Duration::from_secs(3600));
        let empty: HashSet<String> = HashSet::new();

        // First sweep: cid_d is unassigned, starts grace period
        let r1 = gc.mark_and_sweep(&store, &empty, Instant::now()).unwrap();
        assert_eq!(r1.chunks_retained, 1);
        assert_eq!(gc.tracked_count(), 1);

        // Now cid_d is re-assigned
        let mut assigned = HashSet::new();
        assigned.insert("cid_d".to_string());

        let r2 = gc.mark_and_sweep(&store, &assigned, Instant::now()).unwrap();
        assert_eq!(r2.chunks_deleted, 0);
        assert_eq!(gc.tracked_count(), 0); // no longer tracked
        assert!(store.has("cid_d")); // still on disk
    }

    #[test]
    fn gc_l1_unreachable_paused() {
        let (_dir, store) = make_store();
        store.put("cid_e", b"chunk e data").unwrap();

        let assigned: HashSet<String> = HashSet::new();
        let mut gc = GarbageCollector::new(Duration::from_secs(0));

        // Pretend L1 was polled 10 minutes ago
        let stale_poll = Instant::now() - Duration::from_secs(600);
        let result = gc.mark_and_sweep(&store, &assigned, stale_poll).unwrap();

        assert!(result.skipped);
        assert_eq!(result.chunks_deleted, 0);
        assert!(store.has("cid_e")); // NOT deleted
    }

    #[test]
    fn gc_multiple_files() {
        let (_dir, store) = make_store();
        store.put("file_a_chunk", b"file a data").unwrap();
        store.put("file_b_chunk", b"file b data").unwrap();
        store.put("file_c_chunk", b"file c data").unwrap();

        // Assigned to files A and C, but not B
        let mut assigned = HashSet::new();
        assigned.insert("file_a_chunk".to_string());
        assigned.insert("file_c_chunk".to_string());

        let mut gc = GarbageCollector::new(Duration::from_secs(0));
        let result = gc.mark_and_sweep(&store, &assigned, Instant::now()).unwrap();

        assert_eq!(result.chunks_deleted, 1); // only file B's chunk
        assert!(store.has("file_a_chunk"));
        assert!(!store.has("file_b_chunk")); // deleted
        assert!(store.has("file_c_chunk"));
    }

    #[test]
    fn gc_disk_space_reported() {
        let (_dir, store) = make_store();
        let data = vec![0xAA; 4096];
        store.put("cid_f", &data).unwrap();

        let assigned: HashSet<String> = HashSet::new();
        let mut gc = GarbageCollector::new(Duration::from_secs(0));
        let result = gc.mark_and_sweep(&store, &assigned, Instant::now()).unwrap();

        assert_eq!(result.chunks_deleted, 1);
        assert_eq!(result.bytes_freed, 4096);
    }
}
