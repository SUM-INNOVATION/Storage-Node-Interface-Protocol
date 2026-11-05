//! `sum-node repush-audit`: a read-only check, on an archive, that every
//! chunk the chain assigns it can be answered if challenged.
//!
//! For each file of an **explicit inventory** (merkle roots the operator
//! supplies), it reads the chain to find the chunks the file's latest
//! assignment epoch assigns to this archive, and for each one builds the
//! answer the PoR responder would submit, from the local store only
//! ([`crate::por_worker::build_answer`]), and checks it with the chain's own
//! verifier. A chunk is usable when that answer verifies: through the
//! file's Public manifest, or through the leaf and path a V2 push recorded.
//! The answer is built from the stored chunk's bytes, so a missing or
//! altered chunk is not usable either.
//!
//! **The inventory decides what is checked, never the local index.** A file
//! this archive never received is absent from its index, so an empty or
//! partial index cannot show that no repair is needed. With no inventory the
//! verdict is "no inventory", never "complete".
//!
//! **Read-only.** The store is opened with
//! [`ManifestIndex::load_read_only`] and the chunk store is only read; the
//! store's lease is not taken, so the audit can run beside the archive. A
//! push that lands while it runs may not be seen; run it again.
//!
//! What it does not show: that the chain will challenge or reward any chunk,
//! that other archives hold their chunks, or anything about files outside
//! the inventory.

use sum_store::assignment_v2::assigned_archives;
use sum_store::manifest_index::ManifestIndex;
use sum_store::store::ChunkStore;
use sum_store::verify::verify_merkle_proof_bytes_for_tree;

use crate::por_worker::{AnswerSource, build_answer};
use crate::push_validator::V2RpcClient;

/// Exit status of `sum-node repush-audit`, from [`AuditReport::verdict`].
pub mod audit_exit {
    /// Every assigned chunk of every inventory file is usable.
    pub const COMPLETE: i32 = 0;
    /// A chain read failed, or an inventory root is not registered.
    pub const ERROR: i32 = 1;
    /// At least one assigned chunk is not usable.
    pub const INCOMPLETE: i32 = 3;
    /// No inventory was given, so nothing can be shown complete.
    pub const NO_INVENTORY: i32 = 7;
}

/// Whether the responder could answer a challenge on one chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkVerdict {
    /// The answer verifies against the root; how it was built.
    Usable(AnswerSource),
    /// No answer, or one that does not verify; why.
    NotUsable(String),
}

/// One assigned chunk and its verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkAudit {
    pub chunk_index: u32,
    pub verdict: ChunkVerdict,
}

/// The audit of one inventory file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileAudit {
    /// Audited against `epoch`. `chunks` are the chunks that epoch assigns
    /// to this archive (none when it assigns it nothing).
    Audited {
        merkle_root: [u8; 32],
        epoch: u64,
        chunks: Vec<ChunkAudit>,
    },
    /// The chain shows the file `Abandoned`: nothing to answer.
    Abandoned { merkle_root: [u8; 32] },
    /// The chain has no V2 row for the root.
    NotRegistered { merkle_root: [u8; 32] },
    /// A chain read failed.
    Error {
        merkle_root: [u8; 32],
        error: String,
    },
}

/// The overall verdict of an audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditVerdict {
    Complete,
    Incomplete,
    Error,
    NoInventory,
}

/// The report of `repush-audit`.
#[derive(Debug, Clone)]
pub struct AuditReport {
    pub archive: [u8; 20],
    pub files: Vec<FileAudit>,
}

impl AuditReport {
    /// No inventory, then any error, then any unusable chunk, else
    /// complete.
    pub fn verdict(&self) -> AuditVerdict {
        if self.files.is_empty() {
            return AuditVerdict::NoInventory;
        }
        if self
            .files
            .iter()
            .any(|f| matches!(f, FileAudit::Error { .. } | FileAudit::NotRegistered { .. }))
        {
            return AuditVerdict::Error;
        }
        let unusable = self.files.iter().any(|f| match f {
            FileAudit::Audited { chunks, .. } => chunks
                .iter()
                .any(|c| matches!(c.verdict, ChunkVerdict::NotUsable(_))),
            _ => false,
        });
        if unusable {
            AuditVerdict::Incomplete
        } else {
            AuditVerdict::Complete
        }
    }

    /// The process exit code ([`audit_exit`]).
    pub fn exit_code(&self) -> i32 {
        match self.verdict() {
            AuditVerdict::Complete => audit_exit::COMPLETE,
            AuditVerdict::Incomplete => audit_exit::INCOMPLETE,
            AuditVerdict::Error => audit_exit::ERROR,
            AuditVerdict::NoInventory => audit_exit::NO_INVENTORY,
        }
    }
}

/// Audit `inventory` for `archive` against the chain (read-only) and the
/// local store. `r` is the chain's `assignment_replication_factor`.
pub async fn audit<C: V2RpcClient + ?Sized>(
    rpc: &C,
    r: u32,
    archive: [u8; 20],
    manifest_idx: &ManifestIndex,
    local: &ChunkStore,
    inventory: &[[u8; 32]],
) -> AuditReport {
    let mut files = Vec::with_capacity(inventory.len());
    let mut seen = std::collections::HashSet::new();
    for merkle_root in inventory {
        if seen.insert(*merkle_root) {
            files.push(audit_file(rpc, r, archive, manifest_idx, local, *merkle_root).await);
        }
    }
    AuditReport { archive, files }
}

async fn audit_file<C: V2RpcClient + ?Sized>(
    rpc: &C,
    r: u32,
    archive: [u8; 20],
    manifest_idx: &ManifestIndex,
    local: &ChunkStore,
    merkle_root: [u8; 32],
) -> FileAudit {
    let root_hex = format!("0x{}", hex::encode(merkle_root));
    let error = |e: anyhow::Error| FileAudit::Error {
        merkle_root,
        error: format!("{e:#}"),
    };
    let info = match rpc.storage_get_file_info_v2(&root_hex).await {
        Ok(Some(info)) => info,
        Ok(None) => return FileAudit::NotRegistered { merkle_root },
        Err(e) => return error(e.context("storage_getFileInfoV2")),
    };
    if info.lifecycle.is_abandoned() {
        return FileAudit::Abandoned { merkle_root };
    }
    // The epoch archives validate pushes against (the push validator's
    // rule): the latest reassignment epoch, else the assignment height.
    let epoch = match rpc.latest_assignment_epoch(&root_hex).await {
        Ok(None) => info.assignment_height,
        Ok(Some(latest)) if latest < info.assignment_height => {
            return error(anyhow::anyhow!(
                "latest assignment epoch {latest} below assignment_height {}",
                info.assignment_height
            ));
        }
        Ok(Some(latest)) => latest,
        Err(e) => return error(e.context("latest assignment epoch")),
    };
    let raw = match rpc.storage_get_active_nodes_at_height(epoch).await {
        Ok(raw) => raw,
        Err(e) => return error(e.context("storage_getActiveNodesAtHeight")),
    };
    let mut snapshot = Vec::new();
    for record in sum_types::rpc_types::filter_active_archives(raw) {
        match sum_net::l1_address_from_base58(&record.address) {
            Ok(a) => snapshot.push(a),
            Err(e) => return error(e.context("snapshot address decode")),
        }
    }
    snapshot.sort();
    snapshot.dedup();

    let mut chunks = Vec::new();
    for chunk_index in 0..info.chunk_count {
        if !assigned_archives(&merkle_root, &snapshot, chunk_index, r).contains(&archive) {
            continue;
        }
        let verdict = match build_answer(manifest_idx, local, &merkle_root, chunk_index) {
            Ok(answer)
                if verify_merkle_proof_bytes_for_tree(
                    &answer.chunk_hash,
                    chunk_index,
                    &answer.merkle_path,
                    &merkle_root,
                    info.chunk_count,
                ) =>
            {
                ChunkVerdict::Usable(answer.source)
            }
            Ok(_) => ChunkVerdict::NotUsable(
                "the answer does not verify against the root: the stored chunk does not \
                 match it"
                    .into(),
            ),
            Err(e) => ChunkVerdict::NotUsable(format!("{e:#}")),
        };
        chunks.push(ChunkAudit {
            chunk_index,
            verdict,
        });
    }
    FileAudit::Audited {
        merkle_root,
        epoch,
        chunks,
    }
}

/// Parse an inventory file: one merkle root per line (64 hex characters,
/// `0x` optional); blank lines and lines starting with `#` are ignored.
pub fn parse_inventory(text: &str) -> anyhow::Result<Vec<[u8; 32]>> {
    let mut roots = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        roots.push(parse_root(line).map_err(|e| anyhow::anyhow!("line {}: {e}", n + 1))?);
    }
    Ok(roots)
}

/// Parse one merkle root: 64 hex characters, `0x` optional.
pub fn parse_root(s: &str) -> anyhow::Result<[u8; 32]> {
    let hex_str = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(hex_str).map_err(|e| anyhow::anyhow!("not hex: {e}"))?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("a merkle root is 32 bytes, got {}", bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inventory_file_lists_roots_one_per_line() {
        let a = [0xABu8; 32];
        let b = [0x01u8; 32];
        let text = format!(
            "# roots to audit\n\n0x{}\n  {}  \n# done\n",
            hex::encode(a),
            hex::encode(b)
        );
        assert_eq!(parse_inventory(&text).unwrap(), vec![a, b]);
        assert!(parse_inventory("").unwrap().is_empty());
    }

    #[test]
    fn a_malformed_inventory_line_is_refused_with_its_number() {
        let e = parse_inventory(&format!("0x{}\nzz\n", hex::encode([1u8; 32]))).unwrap_err();
        assert!(e.to_string().starts_with("line 2:"), "{e}");
        let e = parse_root("0xabcd").unwrap_err();
        assert!(e.to_string().contains("32 bytes"), "{e}");
    }

    #[test]
    fn no_inventory_is_never_complete() {
        let report = AuditReport {
            archive: [0; 20],
            files: vec![],
        };
        assert_eq!(report.verdict(), AuditVerdict::NoInventory);
        assert_eq!(report.exit_code(), audit_exit::NO_INVENTORY);
    }
}
