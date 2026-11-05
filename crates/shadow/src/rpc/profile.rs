//! Provider profiles: what an endpoint supports and how it limits, written down offline so
//! `shadow plan` can estimate a run without touching the network (RFC-0004 Appendix C).

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::Error;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub provider: Provider,
    #[serde(default)]
    pub methods: Methods,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub billing: Billing,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// A name for reports. Never the URL.
    pub alias: String,
    pub chain_id: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Methods {
    #[serde(default = "yes")]
    pub get_block_by_number: bool,
    #[serde(default = "yes")]
    pub get_block_receipts: bool,
    /// Recorded for completeness. The Shadow never falls back to per-transaction receipts on its
    /// own; that would multiply the bill and needs its own budget (RFC-0002 P4).
    #[serde(default)]
    pub get_transaction_receipt: bool,
    /// Blocks per JSON-RPC batch; 0 sends single calls.
    #[serde(default)]
    pub batch_max: usize,
}

impl Default for Methods {
    fn default() -> Self {
        Self {
            get_block_by_number: true,
            get_block_receipts: true,
            get_transaction_receipt: false,
            batch_max: 0,
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub requests_per_second: Option<f64>,
    pub concurrency: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Billing {
    /// `request` or `cu`. Informational; the budgets are what bound a run.
    #[serde(default)]
    pub unit: Option<String>,
    /// Billing units per method call. Without it a `--budget-cu` run is refused, because a CU cap
    /// over an unknown price bounds nothing.
    #[serde(default)]
    pub cu_per_method: Option<u64>,
}

impl Profile {
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Message(format!("reading {}: {e}", path.display())))?;
        toml::from_str(&text).map_err(|e| Error::Message(format!("{}: {e}", path.display())))
    }

    /// Two methods per block, plus the chain id and finality probes.
    pub fn requests_for(blocks: u64) -> u64 {
        blocks.saturating_mul(2).saturating_add(PROBES)
    }
}

pub const PROBES: u64 = 2;
