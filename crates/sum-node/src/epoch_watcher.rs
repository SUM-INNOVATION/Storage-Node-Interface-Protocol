//! Assignment-epoch watcher (sum-chain #129 prerequisites P1 and P4).
//!
//! A `ReassignChunksV2` adds an assignment epoch to a V2 file. From then
//! on the chain accepts `AcceptAssignmentV2` only against the new epoch's
//! snapshot, and attestations from earlier epochs no longer describe who
//! holds the file. Nothing tells an archive that this happened, so this
//! worker polls committed chain state for every V2 file the archive
//! holds chunks of and, when a file's latest epoch changes, attests
//! `held ∩ assigned(latest epoch)` again through [`run_attestation`].
//!
//! ## What one tick does
//!
//! For each root in the held tracker:
//!
//! 1. `storage_getAssignmentCoverageV2`. No row means the root is not a
//!    V2 file (for example a V1 file whose manifest this archive also
//!    keeps); it is not polled again until restart. A chain that reports
//!    no epoch list has no reassignment epochs; nothing is watched.
//! 2. `storage_getFileInfoV2` for its shape and lifecycle. Abandoned files
//!    are skipped: the chain refuses attestations for them.
//! 3. If the latest epoch equals the one last handled, nothing to do.
//! 4. If it changed since the last tick, attest again.
//! 5. The first time a root is seen (startup, or a restart that may have
//!    missed a reassignment), attest only if the chain's attested count
//!    for this archive in the latest epoch is below the number of chunks
//!    it would attest now. The chain reports counts, not indices, so a
//!    count that is high enough is taken as done.
//!
//! An epoch change is therefore acted on at the first tick after it is
//! committed, unless an RPC fails, in which case the next tick retries.
//! A run that does not finish is retried on later ticks, at most
//! [`MAX_ATTEMPTS_PER_EPOCH`] times per (file, epoch); after that the file
//! waits for its next epoch change or a restart, so a transaction the
//! chain keeps refusing does not repeat every tick.
//!
//! ## What it does not do
//!
//! It attests only chunks already on disk. A newly assigned archive that
//! holds nothing of the file submits nothing until the chunks arrive (an
//! owner push today; an archive-driven re-fetch is separate work, P3).
//! It changes no chain state other than the archive's own attestations,
//! uses the archive's existing key and attestor, and makes no claim about
//! how soon after a reassignment coverage returns (P5).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use sum_store::assignment_v2::chunks_for_archive_v2;
use sum_types::rpc_types::AssignmentCoverageV2;
use tracing::{debug, info, warn};

use crate::assignment_attestor::{AssignmentAttestor, AttestorRpc};
use crate::inbound_v2::{
    AttestTriggerRpc, FileShape, V2Dispatcher, epoch_from_coverage, run_attestation,
};
use crate::push_validator::{V2Params, V2RpcClient};

/// Default poll interval: an epoch change is acted on within this long
/// after it is committed (plus one attestation's finality wait).
pub const DEFAULT_EPOCH_WATCH_INTERVAL: Duration = Duration::from_secs(60);

/// Attestation attempts per (file, epoch) before the watcher waits for
/// the next epoch change or a restart.
pub const MAX_ATTEMPTS_PER_EPOCH: u32 = 3;

/// The held tracker, as the watcher reads it.
pub trait HeldSource: Send + Sync {
    /// Every root with at least one held chunk.
    fn held_roots(&self) -> Vec<[u8; 32]>;
    /// The chunk indices held for `root`.
    fn held_for(&self, root: &[u8; 32]) -> BTreeSet<u32>;
}

impl<V, A, T> HeldSource for V2Dispatcher<V, A, T>
where
    V: V2RpcClient + 'static,
    A: AttestorRpc + 'static,
    T: AttestTriggerRpc + 'static,
{
    fn held_roots(&self) -> Vec<[u8; 32]> {
        V2Dispatcher::held_roots(self)
    }
    fn held_for(&self, root: &[u8; 32]) -> BTreeSet<u32> {
        V2Dispatcher::held_for(self, root)
    }
}

/// What one tick did, for logs and tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// Roots whose coverage was read.
    pub checked: usize,
    /// `(root, epoch)` for each attestation run that finished.
    pub attested: Vec<([u8; 32], u64)>,
    /// Roots left for a later tick (an RPC failed or a run did not finish).
    pub retry: Vec<[u8; 32]>,
}

#[derive(Debug, Clone, Copy)]
struct EpochState {
    epoch: u64,
    /// Whether this epoch has been handled (attested, or nothing to do).
    done: bool,
    attempts: u32,
}

pub struct EpochWatcher<A, T>
where
    A: AttestorRpc + 'static,
    T: AttestTriggerRpc + 'static,
{
    attestor: Arc<AssignmentAttestor<A>>,
    rpc: Arc<T>,
    held: Arc<dyn HeldSource>,
    my_addr: [u8; 20],
    my_addr_base58: String,
    params: V2Params,
    attest_poll_interval: Duration,
    attest_batch_timeout: Duration,
    state: HashMap<[u8; 32], EpochState>,
    not_v2: HashSet<[u8; 32]>,
}

impl<A, T> EpochWatcher<A, T>
where
    A: AttestorRpc + 'static,
    T: AttestTriggerRpc + 'static,
{
    pub fn new(
        attestor: Arc<AssignmentAttestor<A>>,
        rpc: Arc<T>,
        held: Arc<dyn HeldSource>,
        my_addr: [u8; 20],
        params: V2Params,
    ) -> Self {
        Self {
            attestor,
            rpc,
            held,
            my_addr,
            my_addr_base58: sum_net::l1_address_base58(&my_addr),
            params,
            attest_poll_interval: crate::inbound_v2::DEFAULT_ATTEST_POLL_INTERVAL,
            attest_batch_timeout: crate::inbound_v2::DEFAULT_ATTEST_BATCH_TIMEOUT,
            state: HashMap::new(),
            not_v2: HashSet::new(),
        }
    }

    /// For tests that need attestation to finish sooner.
    pub fn with_attest_timing(mut self, poll_interval: Duration, batch_timeout: Duration) -> Self {
        self.attest_poll_interval = poll_interval;
        self.attest_batch_timeout = batch_timeout;
        self
    }

    /// Poll every `interval` until `shutdown` changes. A zero interval
    /// disables the watcher.
    pub async fn run(
        mut self,
        interval: Duration,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        if interval.is_zero() {
            info!("epoch watcher disabled (interval 0)");
            return;
        }
        info!(interval_secs = interval.as_secs(), "epoch watcher started");
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let report = self.tick().await;
                    if !report.attested.is_empty() || !report.retry.is_empty() {
                        info!(
                            checked = report.checked,
                            attested = report.attested.len(),
                            retry = report.retry.len(),
                            "epoch watcher tick"
                        );
                    }
                }
                _ = shutdown.changed() => {
                    info!("epoch watcher stopping");
                    return;
                }
            }
        }
    }

    /// One pass over the held tracker. See the module docs.
    pub async fn tick(&mut self) -> TickReport {
        let mut report = TickReport::default();
        for root in self.held.held_roots() {
            if self.not_v2.contains(&root) {
                continue;
            }
            match self.check(root, &mut report).await {
                Ok(()) => {}
                Err(e) => {
                    warn!(root = %hex::encode(root), %e, "epoch watcher: will retry");
                    report.retry.push(root);
                }
            }
        }
        report
    }

    async fn check(&mut self, root: [u8; 32], report: &mut TickReport) -> anyhow::Result<()> {
        let root_hex = hex::encode(root);
        let Some(coverage) = self.rpc.fetch_coverage(&root).await? else {
            debug!(root = %root_hex, "epoch watcher: no V2 coverage row; not a V2 file");
            self.not_v2.insert(root);
            return Ok(());
        };
        report.checked += 1;
        if coverage.resolved_latest_epoch().is_none() {
            // No epoch list: this chain has no reassignment epochs.
            return Ok(());
        }
        let shape = self.rpc.fetch_file_shape(&root).await?;
        if shape.lifecycle.is_abandoned() {
            return Ok(());
        }
        let epoch = epoch_from_coverage(&coverage, shape.assignment_height)?;

        let previous = self.state.get(&root).copied();
        let mut st = match previous {
            Some(st) if st.epoch == epoch && st.done => return Ok(()),
            Some(st) if st.epoch == epoch => st,
            _ => EpochState {
                epoch,
                done: false,
                attempts: 0,
            },
        };
        if st.attempts >= MAX_ATTEMPTS_PER_EPOCH {
            return Ok(());
        }

        let held = self.held.held_for(&root);
        let first_sight = previous.is_none();
        if held.is_empty()
            || (first_sight
                && self
                    .already_attested(&root, &coverage, &shape, epoch, &held)
                    .await?)
        {
            st.done = true;
            self.state.insert(root, st);
            return Ok(());
        }

        if !first_sight {
            info!(root = %root_hex, epoch, "epoch watcher: assignment epoch changed; attesting again");
        }
        st.attempts += 1;
        let summary = run_attestation(
            self.attestor.clone(),
            self.rpc.clone(),
            root,
            held,
            self.my_addr_base58.clone(),
            self.attest_poll_interval,
            self.attest_batch_timeout,
        )
        .await;
        match summary {
            Some(s) if s.fully_attested() => {
                st.done = true;
                report.attested.push((root, epoch));
            }
            _ => {
                if st.attempts >= MAX_ATTEMPTS_PER_EPOCH {
                    warn!(
                        root = %root_hex,
                        epoch,
                        attempts = st.attempts,
                        "epoch watcher: attestation did not finish; waiting for the next epoch change or a restart"
                    );
                }
                report.retry.push(root);
            }
        }
        self.state.insert(root, st);
        Ok(())
    }

    /// Whether the chain already counts at least as many attested chunks
    /// for this archive in `epoch` as it would attest now.
    async fn already_attested(
        &self,
        root: &[u8; 32],
        coverage: &AssignmentCoverageV2,
        shape: &FileShape,
        epoch: u64,
        held: &BTreeSet<u32>,
    ) -> anyhow::Result<bool> {
        let attested = coverage
            .per_epoch
            .iter()
            .find(|e| e.epoch_height == epoch)
            .and_then(|e| {
                e.per_archive
                    .iter()
                    .find(|a| a.archive == self.my_addr_base58)
            })
            .map(|a| a.attested_count)
            .unwrap_or(0);
        let snapshot = self.rpc.fetch_snapshot(epoch).await?;
        let assigned = chunks_for_archive_v2(
            root,
            shape.chunk_count,
            &snapshot,
            self.params.assignment_replication_factor,
            &self.my_addr,
        );
        let wanted = assigned.intersection(held).count();
        Ok(wanted == 0 || attested as usize >= wanted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use sum_net::l1_address_base58;
    use sum_types::rpc_types::{
        ArchiveCoverageSummaryV2, AssignmentEpochCoverageV2, LifecycleV2, TxStatusV2, VisibilityV2,
    };
    use sumchain_wire::storage_metadata::StorageMetadataOperationV2;
    use sumchain_wire::transaction::{SignedTransaction, TxInner, TxPayload};

    const ROOT: [u8; 32] = [0x31; 32];
    const CHUNKS: u32 = 64;
    const R: u32 = 3;

    fn archives() -> Vec<[u8; 20]> {
        (0..5u8)
            .map(|i| {
                let mut a = [0u8; 20];
                a[0] = 0xA0 + i;
                a
            })
            .collect()
    }

    /// A chain where `ROOT` has `epochs`, each with its own snapshot, and
    /// per-epoch attested counts for each archive.
    #[derive(Default)]
    struct Chain {
        lifecycle: StdMutex<Option<LifecycleV2>>,
        epochs: StdMutex<Vec<u64>>,
        attested: StdMutex<HashMap<(u64, String), u32>>,
        snapshots: StdMutex<HashMap<u64, Vec<[u8; 20]>>>,
        coverage_fails: StdMutex<u32>,
        no_row: StdMutex<bool>,
        legacy: StdMutex<bool>,
        sent: StdMutex<Vec<String>>,
        statuses: StdMutex<VecDeque<TxStatusV2>>,
        coverage_reads: StdMutex<u32>,
    }

    impl Chain {
        fn with_epochs(epochs: &[(u64, Vec<[u8; 20]>)]) -> Arc<Self> {
            let c = Chain::default();
            *c.lifecycle.lock().unwrap() = Some(LifecycleV2::ACTIVE);
            for (h, snap) in epochs {
                c.epochs.lock().unwrap().push(*h);
                c.snapshots.lock().unwrap().insert(*h, snap.clone());
            }
            Arc::new(c)
        }
        fn reassign(&self, height: u64, snapshot: Vec<[u8; 20]>) {
            self.epochs.lock().unwrap().push(height);
            self.snapshots.lock().unwrap().insert(height, snapshot);
        }
        fn finalize_next(&self, n: usize) {
            for _ in 0..n {
                self.statuses
                    .lock()
                    .unwrap()
                    .push_back(TxStatusV2::Finalized { block_height: 1 });
            }
        }
        /// The chunk indices of every `AcceptAssignmentV2` sent so far.
        fn sent_indices(&self) -> Vec<BTreeSet<u32>> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(
                    |hex| match SignedTransaction::from_hex(hex).unwrap().inner {
                        TxInner::V2(tx) => match tx.payload {
                            TxPayload::StorageMetadataV2(d) => match d.operation {
                                StorageMetadataOperationV2::AcceptAssignmentV2 {
                                    chunk_indices,
                                    ..
                                } => chunk_indices.into_iter().collect(),
                                other => panic!("unexpected op {other:?}"),
                            },
                            other => panic!("unexpected payload {other:?}"),
                        },
                        other => panic!("unexpected tx {other:?}"),
                    },
                )
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl crate::tx_wait::TxStatusSource for Chain {
        async fn get_transaction_status(&self, _h: &str) -> anyhow::Result<TxStatusV2> {
            self.statuses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("no status queued"))
        }
    }

    #[async_trait::async_trait]
    impl AttestorRpc for Chain {
        async fn send_raw_transaction(&self, hex: &str) -> anyhow::Result<String> {
            self.sent.lock().unwrap().push(hex.to_string());
            Ok(format!("0xtx{}", self.sent.lock().unwrap().len()))
        }
    }

    #[async_trait::async_trait]
    impl AttestTriggerRpc for Chain {
        async fn fetch_file_shape(&self, root: &[u8; 32]) -> anyhow::Result<FileShape> {
            assert_eq!(root, &ROOT);
            Ok(FileShape {
                chunk_count: CHUNKS,
                assignment_height: self.epochs.lock().unwrap()[0],
                visibility: VisibilityV2::PUBLIC,
                lifecycle: self.lifecycle.lock().unwrap().unwrap(),
            })
        }
        async fn fetch_snapshot(&self, height: u64) -> anyhow::Result<Vec<[u8; 20]>> {
            let mut s = self
                .snapshots
                .lock()
                .unwrap()
                .get(&height)
                .cloned()
                .unwrap();
            s.sort();
            Ok(s)
        }
        async fn fetch_nonce(&self, _addr: &str) -> anyhow::Result<u64> {
            Ok(self.sent.lock().unwrap().len() as u64)
        }
        async fn fetch_coverage(
            &self,
            _root: &[u8; 32],
        ) -> anyhow::Result<Option<AssignmentCoverageV2>> {
            *self.coverage_reads.lock().unwrap() += 1;
            {
                let mut fails = self.coverage_fails.lock().unwrap();
                if *fails > 0 {
                    *fails -= 1;
                    anyhow::bail!("coverage transport failure");
                }
            }
            if *self.no_row.lock().unwrap() {
                return Ok(None);
            }
            let epochs = if *self.legacy.lock().unwrap() {
                vec![]
            } else {
                self.epochs.lock().unwrap().clone()
            };
            let attested = self.attested.lock().unwrap();
            let per_epoch = epochs
                .iter()
                .map(|h| AssignmentEpochCoverageV2 {
                    epoch_height: *h,
                    is_epoch_zero: Some(h) == epochs.first(),
                    covered_count: 0,
                    per_archive: attested
                        .iter()
                        .filter(|((eh, _), _)| eh == h)
                        .map(|((_, a), n)| ArchiveCoverageSummaryV2 {
                            archive: a.clone(),
                            assigned_count: None,
                            attested_count: *n,
                            currently_active: true,
                        })
                        .collect(),
                })
                .collect();
            Ok(Some(AssignmentCoverageV2 {
                chunk_count: CHUNKS,
                covered_count: 0,
                can_activate_now: false,
                missing_total: CHUNKS,
                missing_offset: 0,
                missing_indices: vec![],
                per_archive: vec![],
                latest_assignment_epoch: epochs.last().copied().unwrap_or(0),
                assignment_epochs: epochs,
                reassignment_needed: false,
                per_epoch,
            }))
        }
    }

    struct Held(StdMutex<HashMap<[u8; 32], BTreeSet<u32>>>);
    impl HeldSource for Held {
        fn held_roots(&self) -> Vec<[u8; 32]> {
            self.0.lock().unwrap().keys().copied().collect()
        }
        fn held_for(&self, root: &[u8; 32]) -> BTreeSet<u32> {
            self.0
                .lock()
                .unwrap()
                .get(root)
                .cloned()
                .unwrap_or_default()
        }
    }

    fn assigned(snapshot: &[[u8; 20]], me: &[u8; 20]) -> BTreeSet<u32> {
        let mut s = snapshot.to_vec();
        s.sort();
        chunks_for_archive_v2(&ROOT, CHUNKS, &s, R, me)
    }

    /// `AssignmentAttestor` takes its RPC by value: share the chain.
    struct ChainRef(Arc<Chain>);
    #[async_trait::async_trait]
    impl crate::tx_wait::TxStatusSource for ChainRef {
        async fn get_transaction_status(&self, h: &str) -> anyhow::Result<TxStatusV2> {
            self.0.get_transaction_status(h).await
        }
    }
    #[async_trait::async_trait]
    impl AttestorRpc for ChainRef {
        async fn send_raw_transaction(&self, hex: &str) -> anyhow::Result<String> {
            self.0.send_raw_transaction(hex).await
        }
    }

    fn watcher(
        chain: &Arc<Chain>,
        me: [u8; 20],
        held: BTreeSet<u32>,
    ) -> EpochWatcher<ChainRef, Chain> {
        let attestor = Arc::new(AssignmentAttestor::new(
            ChainRef(chain.clone()),
            [9u8; 32],
            me,
            1337,
            1,
            V2Params {
                assignment_replication_factor: R,
                max_chunk_indices_per_tx: 1024,
            },
        ));
        let held: Arc<dyn HeldSource> =
            Arc::new(Held(StdMutex::new(HashMap::from([(ROOT, held)]))));
        EpochWatcher::new(
            attestor,
            chain.clone(),
            held,
            me,
            V2Params {
                assignment_replication_factor: R,
                max_chunk_indices_per_tx: 1024,
            },
        )
        .with_attest_timing(Duration::from_millis(1), Duration::from_secs(2))
    }

    /// Epoch 1's snapshot: archive 4 left, `0xF0…` joined.
    fn epoch1_snapshot() -> Vec<[u8; 20]> {
        let mut s = archives();
        s[4] = [0xF0; 20];
        s
    }

    fn all_chunks() -> BTreeSet<u32> {
        (0..CHUNKS).collect()
    }

    fn set_attested(chain: &Chain, epoch: u64, me: &[u8; 20], n: usize) {
        chain
            .attested
            .lock()
            .unwrap()
            .insert((epoch, l1_address_base58(me)), n as u32);
    }

    /// P1: a reassignment committed after the watcher started is acted on
    /// at the next tick, with exactly the latest epoch's assignment.
    #[tokio::test]
    async fn an_epoch_change_is_attested_at_the_next_tick() {
        let me = archives()[0];
        let chain = Chain::with_epochs(&[(500, archives())]);
        set_attested(&chain, 500, &me, assigned(&archives(), &me).len());
        let mut w = watcher(&chain, me, all_chunks());

        let first = w.tick().await;
        assert_eq!(first.checked, 1);
        assert!(first.attested.is_empty(), "epoch 0 already attested");
        assert!(chain.sent.lock().unwrap().is_empty());

        chain.reassign(900, epoch1_snapshot());
        chain.finalize_next(1);
        let second = w.tick().await;
        assert_eq!(second.attested, vec![(ROOT, 900)]);
        assert_eq!(
            chain.sent_indices(),
            vec![assigned(&epoch1_snapshot(), &me)]
        );

        let third = w.tick().await;
        assert!(third.attested.is_empty() && third.retry.is_empty());
        assert_eq!(
            chain.sent.lock().unwrap().len(),
            1,
            "handled once per epoch"
        );
    }

    /// Restart after a reassignment the node missed: the chain counts no
    /// attestation from this archive in the latest epoch, so it attests.
    #[tokio::test]
    async fn a_restarted_watcher_attests_an_epoch_it_missed() {
        let me = archives()[1];
        let chain = Chain::with_epochs(&[(500, archives()), (900, epoch1_snapshot())]);
        set_attested(&chain, 500, &me, assigned(&archives(), &me).len());
        chain.finalize_next(1);
        let mut w = watcher(&chain, me, all_chunks());
        let report = w.tick().await;
        assert_eq!(report.attested, vec![(ROOT, 900)]);
        assert_eq!(
            chain.sent_indices(),
            vec![assigned(&epoch1_snapshot(), &me)]
        );
    }

    /// Restart when the latest epoch is already attested: nothing is sent.
    #[tokio::test]
    async fn a_restarted_watcher_does_not_repeat_a_counted_attestation() {
        let me = archives()[1];
        let chain = Chain::with_epochs(&[(500, archives()), (900, epoch1_snapshot())]);
        set_attested(&chain, 900, &me, assigned(&epoch1_snapshot(), &me).len());
        let mut w = watcher(&chain, me, all_chunks());
        let report = w.tick().await;
        assert!(report.attested.is_empty() && report.retry.is_empty());
        assert!(chain.sent.lock().unwrap().is_empty());
    }

    /// An archive the reassignment removed submits nothing.
    #[tokio::test]
    async fn a_departed_archive_submits_nothing_after_a_reassignment() {
        let me = archives()[4];
        let chain = Chain::with_epochs(&[(500, archives())]);
        set_attested(&chain, 500, &me, assigned(&archives(), &me).len());
        let mut w = watcher(&chain, me, all_chunks());
        w.tick().await;
        chain.reassign(900, epoch1_snapshot());
        let report = w.tick().await;
        assert!(report.retry.is_empty());
        assert!(chain.sent.lock().unwrap().is_empty());
    }

    /// A failed read is retried at the next tick, not skipped.
    #[tokio::test]
    async fn a_failed_coverage_read_is_retried() {
        let me = archives()[0];
        let chain = Chain::with_epochs(&[(500, archives()), (900, epoch1_snapshot())]);
        *chain.coverage_fails.lock().unwrap() = 1;
        chain.finalize_next(1);
        let mut w = watcher(&chain, me, all_chunks());
        let first = w.tick().await;
        assert_eq!(first.retry, vec![ROOT]);
        assert!(chain.sent.lock().unwrap().is_empty());
        let second = w.tick().await;
        assert_eq!(second.attested, vec![(ROOT, 900)]);
    }

    /// No V2 coverage row (a V1 file's manifest): not polled again.
    #[tokio::test]
    async fn a_root_with_no_v2_row_is_not_polled_again() {
        let chain = Chain::with_epochs(&[(500, archives())]);
        *chain.no_row.lock().unwrap() = true;
        let mut w = watcher(&chain, archives()[0], all_chunks());
        assert_eq!(w.tick().await.checked, 0);
        w.tick().await;
        assert_eq!(*chain.coverage_reads.lock().unwrap(), 1);
    }

    /// A chain without an epoch list has no reassignment epochs.
    #[tokio::test]
    async fn no_epoch_list_means_nothing_to_watch() {
        let chain = Chain::with_epochs(&[(500, archives())]);
        *chain.legacy.lock().unwrap() = true;
        let mut w = watcher(&chain, archives()[0], all_chunks());
        let report = w.tick().await;
        assert!(report.attested.is_empty() && report.retry.is_empty());
        assert!(chain.sent.lock().unwrap().is_empty());
    }

    /// Abandoned files are skipped: the chain refuses attestations.
    #[tokio::test]
    async fn abandoned_files_are_skipped() {
        let chain = Chain::with_epochs(&[(500, archives()), (900, epoch1_snapshot())]);
        *chain.lifecycle.lock().unwrap() = Some(LifecycleV2::ABANDONED);
        let mut w = watcher(&chain, archives()[0], all_chunks());
        let report = w.tick().await;
        assert!(report.attested.is_empty() && report.retry.is_empty());
        assert!(chain.sent.lock().unwrap().is_empty());
    }

    /// A run that never finishes is retried a bounded number of times.
    #[tokio::test]
    async fn attempts_per_epoch_are_bounded() {
        let me = archives()[0];
        let chain = Chain::with_epochs(&[(500, archives()), (900, epoch1_snapshot())]);
        // No finality status is ever queued: every run times out.
        let mut w = watcher(&chain, me, all_chunks())
            .with_attest_timing(Duration::from_millis(1), Duration::from_millis(30));
        for _ in 0..(MAX_ATTEMPTS_PER_EPOCH + 2) {
            w.tick().await;
        }
        assert_eq!(
            chain.sent.lock().unwrap().len() as u32,
            MAX_ATTEMPTS_PER_EPOCH
        );
        // A new epoch resets the budget.
        let mut s2 = epoch1_snapshot();
        s2[3] = [0xF1; 20];
        chain.reassign(1200, s2);
        chain.finalize_next(1);
        let report = w.tick().await;
        assert_eq!(report.attested, vec![(ROOT, 1200)]);
    }

    /// Interaction with the owner's `sum-node reassign` (N2). The owner tool
    /// submits only while a departed archive holds a chunk under the latest
    /// epoch. Once the chain stamps the new epoch without it, that archive
    /// holds nothing (so the owner tool would not submit again), its watcher
    /// sends nothing, and each surviving archive attests exactly its
    /// new-epoch assignment. A newly assigned archive that holds nothing yet
    /// (there is no re-fetch) sends nothing either.
    #[tokio::test]
    async fn the_owners_reassignment_is_followed_by_latest_epoch_attestations() {
        use sum_store::assignment_v2::first_chunk_assigned_to_any;
        let departed = archives()[4];
        let joiner = [0xF0; 20];
        let new = epoch1_snapshot();
        assert!(
            first_chunk_assigned_to_any(&ROOT, CHUNKS, &archives(), R, &[departed]).is_some(),
            "before: the owner tool's N2 condition holds"
        );
        assert_eq!(
            first_chunk_assigned_to_any(&ROOT, CHUNKS, &new, R, &[departed]),
            None,
            "after: the owner tool would not submit again"
        );

        for me in archives().into_iter().chain([joiner]) {
            let chain = Chain::with_epochs(&[(500, archives()), (900, new.clone())]);
            set_attested(&chain, 500, &me, assigned(&archives(), &me).len());
            chain.finalize_next(1);
            let held = if me == joiner {
                BTreeSet::new()
            } else {
                all_chunks()
            };
            let mut w = watcher(&chain, me, held);
            let report = w.tick().await;
            if me == departed || me == joiner {
                assert!(report.attested.is_empty(), "{me:02x?}");
                assert!(chain.sent.lock().unwrap().is_empty(), "{me:02x?}");
            } else {
                assert_eq!(report.attested, vec![(ROOT, 900)], "{me:02x?}");
                assert_eq!(chain.sent_indices(), vec![assigned(&new, &me)], "{me:02x?}");
            }
        }
    }

    /// No new signing authority: everything the watcher sends is an
    /// `AcceptAssignmentV2` for the held file, sent from the attestor's
    /// existing key's own address and signed by that key over the
    /// transaction's signing hash. The watcher holds no key of its own.
    #[tokio::test]
    async fn watcher_transactions_are_signed_only_by_the_attestors_key() {
        use ed25519_dalek::{Signature, SigningKey};
        let me = archives()[0];
        let chain = Chain::with_epochs(&[(500, archives()), (900, epoch1_snapshot())]);
        chain.finalize_next(1);
        let mut w = watcher(&chain, me, all_chunks());
        assert_eq!(w.tick().await.attested, vec![(ROOT, 900)]);

        let sent = chain.sent.lock().unwrap().clone();
        assert!(!sent.is_empty());
        let key = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        for hex in sent {
            let signed = SignedTransaction::from_hex(&hex).unwrap();
            assert_eq!(signed.public_key, key.to_bytes(), "the attestor's key");
            let TxInner::V2(tx) = &signed.inner else {
                panic!("expected a V2 transaction");
            };
            assert!(signed.verify_signer(), "sent from the key's own address");
            assert_eq!(
                tx.from,
                sumchain_wire::address::Address::from_public_key(&key.to_bytes())
            );
            key.verify_strict(
                tx.signing_hash().as_bytes(),
                &Signature::from_bytes(&signed.signature),
            )
            .expect("signature verifies over the signing hash");
            match &tx.payload {
                TxPayload::StorageMetadataV2(d) => match &d.operation {
                    StorageMetadataOperationV2::AcceptAssignmentV2 { merkle_root, .. } => {
                        assert_eq!(merkle_root.as_bytes(), &ROOT)
                    }
                    other => panic!("unexpected operation {other:?}"),
                },
                other => panic!("unexpected payload {other:?}"),
            }
        }
    }
}
