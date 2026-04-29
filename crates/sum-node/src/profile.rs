//! Runtime profile gating.
//!
//! `NodeProfile::Production` is the default. Production fails closed on
//! every uncertain ACL path: RPC errors deny, unregistered files deny,
//! CIDs not present in the local manifest index deny.
//!
//! `NodeProfile::Dev` exists for local testing without an L1 chain
//! running. It relaxes the same paths and prints a loud warning at every
//! startup. It is **not** safe for production deployments — a single
//! misconfigured `--profile dev` flag would silently disable access
//! control. The startup banner in `log_profile_banner` is intentionally
//! noisy for that reason.

use clap::ValueEnum;
use tracing::{info, warn};

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeProfile {
    Production,
    Dev,
}

impl NodeProfile {
    /// Whether the current profile is Production. Hot path on every ACL
    /// check, so kept const-foldable.
    pub const fn is_production(self) -> bool {
        matches!(self, NodeProfile::Production)
    }

    /// Whether the current profile is Dev.
    pub const fn is_dev(self) -> bool {
        matches!(self, NodeProfile::Dev)
    }
}

/// Print the startup banner. In Dev, this is a `warn!` line that's
/// hard to miss in logs.
pub fn log_profile_banner(profile: NodeProfile) {
    match profile {
        NodeProfile::Production => {
            info!("profile=production (strict ACL, L1 required)");
        }
        NodeProfile::Dev => {
            warn!(
                "⚠️  profile=dev — ACL fail-open on RPC error, unregistered files allowed, \
                 unknown CIDs allowed. NEVER USE IN PRODUCTION."
            );
        }
    }
}
