//! Exclusive lease on a store root.
//!
//! Naming a store root per node (`--store-dir`) lets an operator keep two
//! nodes apart; it does not stop two nodes from being handed the same root.
//! Nothing downstream notices: the chunk namespace is content-addressed, so
//! two archives writing the same root produce no conflict at write time and
//! then garbage-collect against each other's assignment sets, each deleting
//! chunks the other is obliged to serve. The store also documents itself as
//! "exclusively managed" — `ChunkStore::mmap` relies on it for the safety of
//! the mapping it hands out.
//!
//! The lease makes that claim enforced rather than assumed: `flock` with
//! `LOCK_EX | LOCK_NB` on `<store_dir>/.store.lease`, taken once and held for
//! the lifetime of the [`crate::SumStore`] that took it.
//!
//! `flock` is the right primitive here for three reasons. It is advisory
//! between cooperating processes, which is exactly the population being
//! coordinated. It is scoped to the *open file description*, so the kernel
//! releases it when the handle closes — including on a crash or `kill -9`,
//! which a lease written as a PID file would not survive. And it needs no
//! cleanup path: there is no stale lease to reap, because there is no state
//! on disk to go stale, only a zero-byte file.
//!
//! # What the lease does NOT cover
//!
//! The lease is held by [`SumStore`], so only code that goes through `SumStore`
//! participates in it. A `ChunkStore` constructed directly takes no lease, is
//! invisible to the lease held by anything else, and will happily read and write
//! a root that another process is actively managing — `flock` constrains only
//! the descriptors that ask for it.
//!
//! That is a deliberate boundary, not an oversight: locking in `ChunkStore::new`
//! would impose a permanent one-handle-per-root constraint on a type constructed
//! freely by `gc`'s tests, by tooling, and by anything wanting a read-only look
//! at a directory. But it means the rule has to be stated rather than assumed:
//!
//! > Do not construct a `ChunkStore` directly against a root that a cooperating
//! > process may be using. Go through `SumStore`, which takes the lease, or use
//! > a root nothing else manages.
//!
//! Verified at the time of writing: the only non-test `ChunkStore::new` in the
//! workspace is the one inside `SumStore::new` itself. Every other call site is
//! inside a `#[cfg(test)]` module or an integration test, each against its own
//! tempdir. There is no production bypass today — the hazard is a future caller,
//! which is why this is written down here rather than left to be rediscovered.
//!
//! The lock is taken in `SumStore::new`, not `ChunkStore::new`. `flock` is
//! per open file description, so locking in `ChunkStore` would impose a
//! permanent one-handle-per-root constraint on a type that is constructed
//! freely — by `gc`'s tests, by tooling, by anything wanting a read-only look
//! at a directory — in exchange for no additional protection: `SumStore` is
//! the type a node runs on.

use std::path::{Path, PathBuf};

use crate::error::{Result, StoreError};

/// Name of the lease file inside the store root.
///
/// Deliberately not `<something>.chunk`: `ChunkStore::list_all_cids` — the
/// sole input to the garbage collector's delete loop — yields only directory
/// entries ending in `.chunk`, so this name is structurally invisible to GC
/// and to every chunk lister built on it.
pub const LEASE_FILE_NAME: &str = ".store.lease";

/// An exclusive, process-lifetime lease on one store root.
///
/// Dropping this releases the lock. It is held as a field of
/// [`crate::SumStore`] and therefore lives as long as the store does.
#[derive(Debug)]
pub struct StoreLease {
    path: PathBuf,
    /// The open file description carrying the `flock`. Never read: holding it
    /// open *is* the lease, and closing it is what releases the lock.
    #[cfg(unix)]
    _file: std::fs::File,
}

impl StoreLease {
    /// Take the exclusive lease on `store_dir`, creating the root and the
    /// lease file if they do not exist.
    ///
    /// Returns [`StoreError::StoreRootBusy`] if another open file description
    /// — in this process or any other — already holds it.
    pub fn acquire(store_dir: &Path) -> Result<Self> {
        // Fail closed BEFORE any filesystem mutation, mirroring
        // `PublicationStore::new`. A target that cannot express the lease must
        // not first create a root as though it could.
        #[cfg(not(unix))]
        {
            let _ = store_dir;
            Err(StoreError::LeaseUnsupportedPlatform)
        }
        #[cfg(unix)]
        {
            std::fs::create_dir_all(store_dir)?;
            let path = store_dir.join(LEASE_FILE_NAME);
            // `truncate(false)` on purpose: the file's contents are not the
            // lease and must never be treated as though they were. Truncating
            // would be a write to a root this process has not yet been granted.
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => Ok(Self { path, _file: file }),
                Err(err) if err == rustix::io::Errno::WOULDBLOCK => {
                    Err(StoreError::StoreRootBusy {
                        root: store_dir.display().to_string(),
                        lease: path.display().to_string(),
                    })
                }
                Err(err) => Err(StoreError::Io(err.into())),
            }
        }
    }

    /// Path of the lease file this lease holds.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Two leases on one root cannot coexist.
    ///
    /// Taken from two open file descriptions in one process, which is what
    /// `flock` actually discriminates on — the kernel draws no distinction
    /// between two descriptions in one process and two in separate processes,
    /// so this is the same contention a second `sum-node` meets.
    #[test]
    fn a_second_lease_on_one_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");

        let _held = StoreLease::acquire(&root).expect("first lease");
        let err = StoreLease::acquire(&root).expect_err("second lease must be refused");

        assert!(
            matches!(err, StoreError::StoreRootBusy { .. }),
            "expected StoreRootBusy, got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("--store-dir"),
            "the error must name the flag that fixes it: {msg}"
        );
    }

    /// Releasing the lease frees the root — no stale state to reap.
    ///
    /// This is the property a PID-file lease would fail: the lock lives in the
    /// open file description, so it is gone the moment the handle closes,
    /// including on an abrupt exit.
    #[test]
    fn dropping_a_lease_frees_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");

        let held = StoreLease::acquire(&root).expect("first lease");
        drop(held);

        StoreLease::acquire(&root).expect("the root is free once the lease is dropped");
    }

    /// Distinct roots do not contend. The lease constrains sharing, not
    /// running several nodes on one host — which is the configuration the
    /// `--store-dir` flag exists to support.
    #[test]
    fn distinct_roots_do_not_contend() {
        let dir = tempfile::tempdir().unwrap();

        let _a = StoreLease::acquire(&dir.path().join("a")).expect("lease on a");
        let _b = StoreLease::acquire(&dir.path().join("b")).expect("lease on b");
        let _c = StoreLease::acquire(&dir.path().join("c")).expect("lease on c");
    }

    /// The lease file lives inside the root it leases, under the documented
    /// name, and is not mistakable for a chunk.
    #[test]
    fn the_lease_file_lives_in_the_root_and_is_not_a_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");

        let lease = StoreLease::acquire(&root).expect("lease");

        assert_eq!(lease.path(), root.join(LEASE_FILE_NAME));
        assert!(lease.path().exists());
        assert!(
            !LEASE_FILE_NAME.ends_with(".chunk"),
            "a `.chunk` suffix would put the lease in GC's delete loop"
        );
    }
}
