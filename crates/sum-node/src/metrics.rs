//! Basic node metrics via atomic counters.
//!
//! No external crate needed — uses `std::sync::atomic` for lock-free counters.
//! Thread an `Arc<NodeMetrics>` through the main event loop and background workers.

use std::sync::atomic::{AtomicU64, Ordering};

/// Atomic counters for node-level metrics.
#[derive(Debug, Default)]
pub struct NodeMetrics {
    pub chunks_served: AtomicU64,
    pub por_proofs_submitted: AtomicU64,
    pub por_proofs_failed: AtomicU64,
    pub gc_chunks_deleted: AtomicU64,
    pub peers_connected: AtomicU64,
}

impl NodeMetrics {
    pub fn inc_chunks_served(&self) {
        self.chunks_served.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_por_submitted(&self) {
        self.por_proofs_submitted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_por_failed(&self) {
        self.por_proofs_failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_gc_deleted(&self, count: u32) {
        self.gc_chunks_deleted.fetch_add(count as u64, Ordering::Relaxed);
    }

    pub fn inc_peers(&self) {
        self.peers_connected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_peers(&self) {
        self.peers_connected.fetch_sub(1, Ordering::Relaxed);
    }

    /// Return a snapshot of all counters (for logging / health endpoints).
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            chunks_served: self.chunks_served.load(Ordering::Relaxed),
            por_proofs_submitted: self.por_proofs_submitted.load(Ordering::Relaxed),
            por_proofs_failed: self.por_proofs_failed.load(Ordering::Relaxed),
            gc_chunks_deleted: self.gc_chunks_deleted.load(Ordering::Relaxed),
            peers_connected: self.peers_connected.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time snapshot of all metrics counters.
#[derive(Debug)]
pub struct MetricsSnapshot {
    pub chunks_served: u64,
    pub por_proofs_submitted: u64,
    pub por_proofs_failed: u64,
    pub gc_chunks_deleted: u64,
    pub peers_connected: u64,
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_increment_and_snapshot() {
        let m = NodeMetrics::default();

        m.inc_chunks_served();
        m.inc_chunks_served();
        m.inc_chunks_served();
        m.inc_por_submitted();
        m.inc_por_submitted();
        m.inc_por_failed();
        m.inc_gc_deleted(5);

        let snap = m.snapshot();
        assert_eq!(snap.chunks_served, 3);
        assert_eq!(snap.por_proofs_submitted, 2);
        assert_eq!(snap.por_proofs_failed, 1);
        assert_eq!(snap.gc_chunks_deleted, 5);
        assert_eq!(snap.peers_connected, 0);
    }

    #[test]
    fn metrics_peers_inc_dec() {
        let m = NodeMetrics::default();

        m.inc_peers();
        m.inc_peers();
        m.inc_peers();
        assert_eq!(m.snapshot().peers_connected, 3);

        m.dec_peers();
        assert_eq!(m.snapshot().peers_connected, 2);

        m.dec_peers();
        m.dec_peers();
        assert_eq!(m.snapshot().peers_connected, 0);
    }

    #[test]
    fn metrics_default_is_zero() {
        let m = NodeMetrics::default();
        let snap = m.snapshot();
        assert_eq!(snap.chunks_served, 0);
        assert_eq!(snap.por_proofs_submitted, 0);
        assert_eq!(snap.por_proofs_failed, 0);
        assert_eq!(snap.gc_chunks_deleted, 0);
        assert_eq!(snap.peers_connected, 0);
    }

    #[test]
    fn metrics_gc_batch_increment() {
        let m = NodeMetrics::default();
        m.inc_gc_deleted(10);
        m.inc_gc_deleted(25);
        assert_eq!(m.snapshot().gc_chunks_deleted, 35);
    }
}
