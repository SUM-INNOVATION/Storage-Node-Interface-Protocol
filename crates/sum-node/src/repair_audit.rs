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
//! **Epochs.** Each file is audited against its latest assignment epoch,
//! read before the file is evaluated, and every file's epoch is read again
//! once all are evaluated. An epoch that moved in between makes the verdict
//! incomplete (the file's chunks were checked against an assignment that no
//! longer holds); a second read that fails makes it an error. A complete
//! verdict covers the epochs it reports, and only those: a later
//! reassignment needs a new repush and a new audit.
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
    /// A chain read failed (including the final read of an epoch), or an
    /// inventory root is not registered.
    pub const ERROR: i32 = 1;
    /// At least one assigned chunk is not usable, or a file's assignment
    /// epoch moved while the audit ran.
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

/// A file's assignment epoch, read again after every file was evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EpochRecheck {
    /// Still the epoch the file was audited against.
    Unchanged,
    /// A reassignment moved the file to epoch `to` meanwhile.
    Moved { to: u64 },
    /// The second read failed; the audit cannot be confirmed.
    Failed(String),
}

/// The audit of one inventory file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileAudit {
    /// Audited against `epoch`. `chunks` are the chunks that epoch assigns
    /// to this archive (none when it assigns it nothing); `recheck` is the
    /// epoch as read again at the end.
    Audited {
        merkle_root: [u8; 32],
        epoch: u64,
        chunks: Vec<ChunkAudit>,
        recheck: EpochRecheck,
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
    /// No inventory; then any error (a failed read, including an epoch's
    /// final read, or an unregistered root); then any unusable chunk or
    /// moved epoch; else complete, for the epochs [`Self::covered_epochs`]
    /// reports.
    pub fn verdict(&self) -> AuditVerdict {
        if self.files.is_empty() {
            return AuditVerdict::NoInventory;
        }
        if self.files.iter().any(|f| {
            matches!(
                f,
                FileAudit::Error { .. }
                    | FileAudit::NotRegistered { .. }
                    | FileAudit::Audited {
                        recheck: EpochRecheck::Failed(_),
                        ..
                    }
            )
        }) {
            return AuditVerdict::Error;
        }
        let incomplete = self.files.iter().any(|f| match f {
            FileAudit::Audited {
                chunks, recheck, ..
            } => {
                *recheck != EpochRecheck::Unchanged
                    || chunks
                        .iter()
                        .any(|c| matches!(c.verdict, ChunkVerdict::NotUsable(_)))
            }
            _ => false,
        });
        if incomplete {
            AuditVerdict::Incomplete
        } else {
            AuditVerdict::Complete
        }
    }

    /// The assignment epochs the audited files were checked against. A
    /// complete verdict holds for these epochs only.
    pub fn covered_epochs(&self) -> std::collections::BTreeSet<u64> {
        self.files
            .iter()
            .filter_map(|f| match f {
                FileAudit::Audited { epoch, .. } => Some(*epoch),
                _ => None,
            })
            .collect()
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
    let mut assignment_heights = Vec::with_capacity(inventory.len());
    let mut seen = std::collections::HashSet::new();
    for merkle_root in inventory {
        if seen.insert(*merkle_root) {
            let (file, assignment_height) =
                audit_file(rpc, r, archive, manifest_idx, local, *merkle_root).await;
            files.push(file);
            assignment_heights.push(assignment_height);
        }
    }
    // Every audited file's epoch again, once all are evaluated: a
    // reassignment during the audit moves what "assigned" means.
    for (file, assignment_height) in files.iter_mut().zip(assignment_heights) {
        if let FileAudit::Audited {
            merkle_root,
            epoch,
            recheck,
            ..
        } = file
        {
            let root_hex = format!("0x{}", hex::encode(*merkle_root));
            *recheck = match read_epoch(rpc, &root_hex, assignment_height).await {
                Ok(now) if now == *epoch => EpochRecheck::Unchanged,
                Ok(now) => EpochRecheck::Moved { to: now },
                Err(e) => EpochRecheck::Failed(format!("{e:#}")),
            };
        }
    }
    AuditReport { archive, files }
}

/// The epoch archives validate pushes against (the push validator's rule):
/// the file's latest reassignment epoch, else its assignment height.
async fn read_epoch<C: V2RpcClient + ?Sized>(
    rpc: &C,
    root_hex: &str,
    assignment_height: u64,
) -> anyhow::Result<u64> {
    match rpc
        .latest_assignment_epoch(root_hex)
        .await
        .map_err(|e| e.context("latest assignment epoch"))?
    {
        None => Ok(assignment_height),
        Some(latest) if latest < assignment_height => anyhow::bail!(
            "latest assignment epoch {latest} below assignment_height {assignment_height}"
        ),
        Some(latest) => Ok(latest),
    }
}

async fn audit_file<C: V2RpcClient + ?Sized>(
    rpc: &C,
    r: u32,
    archive: [u8; 20],
    manifest_idx: &ManifestIndex,
    local: &ChunkStore,
    merkle_root: [u8; 32],
) -> (FileAudit, u64) {
    let root_hex = format!("0x{}", hex::encode(merkle_root));
    let error = |e: anyhow::Error| FileAudit::Error {
        merkle_root,
        error: format!("{e:#}"),
    };
    let info = match rpc.storage_get_file_info_v2(&root_hex).await {
        Ok(Some(info)) => info,
        Ok(None) => return (FileAudit::NotRegistered { merkle_root }, 0),
        Err(e) => return (error(e.context("storage_getFileInfoV2")), 0),
    };
    let assignment_height = info.assignment_height;
    if info.lifecycle.is_abandoned() {
        return (FileAudit::Abandoned { merkle_root }, assignment_height);
    }
    let epoch = match read_epoch(rpc, &root_hex, assignment_height).await {
        Ok(epoch) => epoch,
        Err(e) => return (error(e), assignment_height),
    };
    let raw = match rpc.storage_get_active_nodes_at_height(epoch).await {
        Ok(raw) => raw,
        Err(e) => {
            return (
                error(e.context("storage_getActiveNodesAtHeight")),
                assignment_height,
            );
        }
    };
    let mut snapshot = Vec::new();
    for record in sum_types::rpc_types::filter_active_archives(raw) {
        match sum_net::l1_address_from_base58(&record.address) {
            Ok(a) => snapshot.push(a),
            Err(e) => {
                return (
                    error(e.context("snapshot address decode")),
                    assignment_height,
                );
            }
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
    (
        FileAudit::Audited {
            merkle_root,
            epoch,
            chunks,
            // Set by the second read in `audit`.
            recheck: EpochRecheck::Unchanged,
        },
        assignment_height,
    )
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
