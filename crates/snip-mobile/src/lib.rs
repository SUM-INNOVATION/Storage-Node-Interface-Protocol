//! UniFFI surface for embedding the SNIP client in mobile apps.
//!
//! Walking skeleton: exports a version probe while pulling the full
//! sum-node dependency tree (libp2p/QUIC, reqwest, blake3) so the iOS
//! cross-compilation risk is validated before any client logic lands.

uniffi::setup_scaffolding!();

// Link the client library the real API will wrap.
pub use sum_node as node;

/// Crate version, for a first end-to-end FFI smoke test from Swift.
#[uniffi::export]
pub fn snip_core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
