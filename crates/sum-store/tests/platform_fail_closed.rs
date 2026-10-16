//! Native Windows is not a SNIP store target (SNIP #49). Store publication and
//! the store-root lease must refuse with their unsupported-platform errors, and
//! must refuse before anything is created on disk.
//!
//! Compiled only off Unix: on Unix this test binary is empty. The
//! `windows-fail-closed` mode of `scripts/platform-validation.sh` requires
//! exactly three passing tests here, so an empty run cannot pass on Windows.
#![cfg(not(unix))]

use std::path::PathBuf;

use sum_store::{ChunkStore, StoreError, StoreLease, SumStore};
use sum_types::config::StoreConfig;

/// A store root inside a fresh temporary directory, not yet created.
fn absent_root() -> (tempfile::TempDir, PathBuf) {
    let parent = tempfile::tempdir().expect("temporary directory");
    let root = parent.path().join("store");
    (parent, root)
}

#[test]
fn chunk_store_publication_fails_closed_before_creating_the_root() {
    let (_parent, root) = absent_root();
    let err = ChunkStore::new(root.clone())
        .err()
        .expect("native Windows must refuse store publication");
    assert!(
        matches!(err, StoreError::UnsupportedPlatform),
        "unexpected error: {err:?}"
    );
    assert!(err.to_string().contains("WSL2"), "{err}");
    assert!(!root.exists(), "refused store created {}", root.display());
}

#[test]
fn store_lease_fails_closed_before_creating_the_root() {
    let (_parent, root) = absent_root();
    let err = StoreLease::acquire(&root).expect_err("native Windows must refuse the store lease");
    assert!(
        matches!(err, StoreError::LeaseUnsupportedPlatform),
        "unexpected error: {err:?}"
    );
    assert!(err.to_string().contains("WSL2"), "{err}");
    assert!(!root.exists(), "refused lease created {}", root.display());
}

#[test]
fn sum_store_fails_closed_at_the_lease_before_creating_the_root() {
    let (_parent, root) = absent_root();
    let config = StoreConfig {
        store_dir: root.clone(),
        max_chunk_msg_bytes: 2 * 1024 * 1024,
    };
    let err = SumStore::new(config)
        .err()
        .expect("native Windows must refuse to open a store");
    assert!(
        matches!(err, StoreError::LeaseUnsupportedPlatform),
        "unexpected error: {err:?}"
    );
    assert!(!root.exists(), "refused store created {}", root.display());
}
