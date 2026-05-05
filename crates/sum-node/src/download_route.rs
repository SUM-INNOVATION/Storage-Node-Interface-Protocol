//! Download dispatch routing (WS4 — privacy fail-closed).
//!
//! [`route_download_target`] decides which download path to take for a
//! given chain row response, given **only** the V2 chain row presence
//! and visibility byte. The decision MUST NOT depend on the local
//! chunk store, libp2p peer state, or any other side channel — chain
//! row visibility is the load-bearing input.
//!
//! Wire reality (see [`sum_types::rpc_types::VisibilityV2`]):
//! `VisibilityV2(pub u8)` with `PUBLIC = 0`, `PRIVATE = 1`. Because
//! the field is a `serde(transparent)` newtype over `u8`, ANY byte
//! value the chain emits is deserialize-valid. SNIP MUST NOT silently
//! treat unknown bytes as "Public" (fail-open) — a future chain
//! release adding `Restricted = 2` would otherwise downgrade a new
//! visibility class to public reads. Routing fails closed with a
//! typed [`RouteError::UnknownV2Visibility`] for any byte outside the
//! known set.

use thiserror::Error;

use sum_types::rpc_types::{StorageFileInfoV2, VisibilityV2};

/// The download path the dispatcher should take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadPath {
    /// No V2 row on chain — route to the legacy V1 / pre-V2 download
    /// orchestrator. Public-by-design; no V2 ACL applies.
    V1Legacy,
    /// `Some(StorageFileInfoV2)` with `visibility == PUBLIC`. Route to
    /// the V1 / Public download orchestrator (same path as `V1Legacy`
    /// today; the variant exists so future code can distinguish "V2
    /// confirms Public" from "no V2 row at all").
    V2Public,
    /// `Some(StorageFileInfoV2)` with `visibility == PRIVATE`. Route
    /// to `run_download_private`.
    V2Private,
}

/// Errors that abort routing — caller MUST refuse the download.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RouteError {
    /// V2 row's `visibility` byte is not one of the known variants
    /// (PUBLIC = 0, PRIVATE = 1). Refuse the download — the chain may
    /// have introduced a new visibility class SNIP doesn't know how
    /// to enforce ACLs for.
    #[error(
        "V2 row reports visibility = {raw} which is not a known SNIP \
         variant (expected PUBLIC=0 or PRIVATE=1); refusing to download \
         — upgrade SNIP to a release that supports this visibility class"
    )]
    UnknownV2Visibility { raw: u8 },
}

/// Decide the download path from the chain V2 row.
///
/// Inputs: the optional `StorageFileInfoV2` returned by
/// `storage_getFileInfoV2`. `None` means the row is absent (or the
/// RPC was unavailable AND the caller chose to `.ok()` the error;
/// either way, no V2 metadata exists).
///
/// Outputs: a `DownloadPath` for the three known cases, or a typed
/// `RouteError` for unknown visibility bytes (fail-closed).
pub fn route_download_target(info: Option<&StorageFileInfoV2>) -> Result<DownloadPath, RouteError> {
    let Some(info) = info else {
        return Ok(DownloadPath::V1Legacy);
    };
    match info.visibility {
        VisibilityV2::PUBLIC => Ok(DownloadPath::V2Public),
        VisibilityV2::PRIVATE => Ok(DownloadPath::V2Private),
        VisibilityV2(raw) => Err(RouteError::UnknownV2Visibility { raw }),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sum_types::rpc_types::{AccessEntryV2, LifecycleV2};

    /// Build a minimal `StorageFileInfoV2` for routing tests. Only
    /// `visibility` matters; every other field is set to a benign
    /// default. The test name pins the load-bearing field at the
    /// call site.
    fn fake_info(visibility: VisibilityV2) -> StorageFileInfoV2 {
        StorageFileInfoV2 {
            merkle_root: "0x".into(),
            owner: "owner".into(),
            plaintext_size_bytes: 0,
            stored_size_bytes: 0,
            chunk_count: 0,
            fee_pool: 0,
            created_at: 0,
            activated_at_height: None,
            abandoned_at_height: None,
            assignment_height: 0,
            visibility,
            lifecycle: LifecycleV2::ACTIVE,
            access_list: Vec::<AccessEntryV2>::new(),
        }
    }

    /// `None` → `V1Legacy`. The chain has no V2 metadata for this
    /// root; defer to the legacy / pre-V2 download path.
    #[test]
    fn route_none_is_v1_legacy() {
        assert_eq!(route_download_target(None), Ok(DownloadPath::V1Legacy));
    }

    /// `Some(Public)` → `V2Public`. Chain confirms Public; route to
    /// the same orchestrator as V1 but tag the path explicitly so a
    /// future debug log / metric can distinguish "chain says Public"
    /// from "chain has no V2 row."
    #[test]
    fn route_some_public_is_v2_public() {
        let info = fake_info(VisibilityV2::PUBLIC);
        assert_eq!(
            route_download_target(Some(&info)),
            Ok(DownloadPath::V2Public)
        );
    }

    /// `Some(Private)` → `V2Private`. Chain confirms Private; route
    /// to `run_download_private` for ACL + decryption.
    #[test]
    fn route_some_private_is_v2_private() {
        let info = fake_info(VisibilityV2::PRIVATE);
        assert_eq!(
            route_download_target(Some(&info)),
            Ok(DownloadPath::V2Private)
        );
    }

    /// **Privacy row #9 + #15 fail-closed pin.** Unknown visibility
    /// byte (e.g. chain adds `Restricted = 2` in a future release)
    /// MUST surface as `Err(UnknownV2Visibility)`. Silently routing
    /// to V2Public would let a new visibility class downgrade to
    /// world-readable; routing to V2Private would block legitimate
    /// reads. The honest answer is "I don't know how to enforce this
    /// ACL — refuse."
    #[test]
    fn route_unknown_visibility_byte_fails_closed() {
        for raw in [2u8, 3, 5, 0xff] {
            let info = fake_info(VisibilityV2(raw));
            let err = route_download_target(Some(&info))
                .expect_err("unknown visibility byte must fail closed");
            assert_eq!(err, RouteError::UnknownV2Visibility { raw });
        }
    }

    /// Sanity guard against a future "let's just default unknown to
    /// Public to be permissive" regression.
    #[test]
    fn route_unknown_visibility_does_not_downgrade_to_public() {
        let info = fake_info(VisibilityV2(2));
        match route_download_target(Some(&info)) {
            Ok(p) => panic!(
                "regression: unknown visibility byte routed to {p:?} \
                 (must be UnknownV2Visibility, not a downgrade to Public)"
            ),
            Err(RouteError::UnknownV2Visibility { raw: 2 }) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
}
