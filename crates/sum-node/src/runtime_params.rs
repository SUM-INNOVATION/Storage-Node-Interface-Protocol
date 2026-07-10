//! Chain parameters SNIP has fetched, validated at read time, and pinned
//! for the lifetime of one operation (V2 ingest, download, etc.) or
//! process (long-running `listen`).
//!
//! Chain params are genesis-fixed protocol constants — a change would
//! be a hard fork (see `crates/sum-types/src/rpc_types.rs`
//! `ChainParamsInfo` docstring) — so caching within any single
//! operation is safe.
//!
//! Validation policy MIRRORS the chain exactly. Upstream
//! `sum-chain/crates/primitives/src/storage_metadata.rs:299-302`
//! tolerates `assignment_replication_factor == 0` and returns empty
//! assignments; SNIP does likewise. No invented sanity limits.

use tracing::warn;

use sum_types::rpc_types::ChainParamsInfo;

/// Cached, validated chain parameters SNIP uses for one operation or
/// process. Immutable after construction.
#[derive(Debug, Clone)]
pub struct RuntimeChainParams {
    /// Chain identifier for signing (see `docs/reference/config-flags.md`
    /// "Chain ID safety").
    pub chain_id: u64,
    /// Replication factor consumed by every assignment consumer.
    /// `min(assignment_replication_factor, active_snapshot.len())` is
    /// the effective assignment cardinality per chunk, computed at
    /// planning time.
    pub assignment_replication_factor: u32,
}

impl RuntimeChainParams {
    /// Construct from a live `chain_getChainParams` response. Logs a
    /// WARN if `assignment_replication_factor == 0` so operators see
    /// the degenerate config; the field is passed through unchanged
    /// (chain accepts it, mirrors chain semantics).
    pub fn from_chain(params: &ChainParamsInfo) -> Self {
        if params.assignment_replication_factor == 0 {
            warn!(
                "chain_getChainParams reported assignment_replication_factor = 0; \
                 all assignments will be empty (mirrors chain behavior — see \
                 sum-chain/crates/primitives/src/storage_metadata.rs:299-302). \
                 Fresh V2 ingest is refused via preflight before S1; \
                 short-lived subcommands surface a no-eligible-targets failure."
            );
        }
        Self {
            chain_id: params.chain_id,
            assignment_replication_factor: params.assignment_replication_factor,
        }
    }

    /// Dev-profile fallback ONLY. Callers MUST have logged a WARN
    /// naming the fallback before calling. Production code paths never
    /// call this; the release-checklist audit
    /// (`rg -n '\b(REPLICATION_FACTOR|DEFAULT_REPLICATION_FACTOR)\b'
    /// crates -g '*.rs'`) confirms non-consumption.
    pub fn dev_fallback() -> Self {
        Self {
            // Matches the historical dev-mirror chain_id documented in
            // docs/reference/chain-compat.md. Operators overriding this
            // via --chain-id supersede dev_fallback for tx-signing.
            chain_id: 31337,
            assignment_replication_factor: sum_types::storage::DEFAULT_REPLICATION_FACTOR,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(r: u32) -> ChainParamsInfo {
        ChainParamsInfo {
            chain_id: 1,
            block_time_ms: 3_000,
            finality_depth: 6,
            min_fee: 1_000,
            assignment_replication_factor: r,
            max_chunk_indices_per_tx: 65_536,
            max_chunk_count_per_file: 1_048_576,
            activation_grace_blocks: 50,
            v2_enabled_from_height: Some(0),
        }
    }

    #[test]
    fn runtime_chain_params_from_chain_preserves_r() {
        let rt = RuntimeChainParams::from_chain(&params(5));
        assert_eq!(rt.assignment_replication_factor, 5);
        assert_eq!(rt.chain_id, 1);
    }

    /// R=0 is passed through unchanged (mirrors chain). Downstream
    /// preflight rejects fresh ingest before S1; short-lived
    /// subcommands surface a no-eligible-targets failure via their
    /// native result types.
    #[test]
    fn runtime_chain_params_from_chain_r_zero_preserves_value() {
        let rt = RuntimeChainParams::from_chain(&params(0));
        assert_eq!(rt.assignment_replication_factor, 0);
    }

    /// Reviewer-required invariant: the dev fallback references the
    /// documented `DEFAULT_REPLICATION_FACTOR` and nothing else. This
    /// is the ONE constant reference allowed in production runtime
    /// code; the release-checklist audit tracks it explicitly.
    #[test]
    fn dev_fallback_uses_documented_default_replication_factor() {
        let rt = RuntimeChainParams::dev_fallback();
        #[allow(deprecated)]
        {
            // Also verifies the deprecated alias still resolves to the
            // same value during the compatibility window.
            assert_eq!(
                rt.assignment_replication_factor,
                sum_types::storage::DEFAULT_REPLICATION_FACTOR
            );
            assert_eq!(
                sum_types::storage::REPLICATION_FACTOR,
                sum_types::storage::DEFAULT_REPLICATION_FACTOR
            );
        }
    }
}
