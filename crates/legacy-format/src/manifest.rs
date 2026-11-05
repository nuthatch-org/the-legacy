//! The relic manifest (RFC-0001 §8.3).
//!
//! One manifest per relic. It commits to every file in the relic by BLAKE3, to the relic's block
//! boundary, and to the previous relic's manifest hash, which is what turns a pile of Parquet into
//! a chain two mirrors can compare in a single 32-byte exchange.

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::hash::{blake3, Hash32};
use crate::jcs;
use crate::relic::{relic_index, relic_range, BlockRange, BLOCKS_PER_RELIC};
use crate::{HASH_ALGO, SPEC_VERSION};

/// The tables a relic may contain (§6). `withdrawals` appears from Shapella; `traces` is the
/// optional tier and is **not** header-committed, which §6.8 says out loud and this type cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Table {
    Headers,
    Transactions,
    Receipts,
    Logs,
    Withdrawals,
    Traces,
}

impl Table {
    /// The file name this table is written to inside a relic directory.
    pub fn file_name(self) -> &'static str {
        match self {
            Table::Headers => "headers.parquet",
            Table::Transactions => "transactions.parquet",
            Table::Receipts => "receipts.parquet",
            Table::Logs => "logs.parquet",
            Table::Withdrawals => "withdrawals.parquet",
            Table::Traces => "traces.parquet",
        }
    }

    /// Whether the table's contents can be checked against a field in the block header.
    ///
    /// Traces cannot. No header field commits to a call tree, so no amount of hashing will catch a
    /// fabricated one; only cross-producer agreement or local re-execution will (§16).
    pub fn is_header_committed(self) -> bool {
        !matches!(self, Table::Traces)
    }
}

/// The block-hash boundary of a relic. `parent_hash_of_start` is what links this relic to the
/// previous one, so linkage survives even if the relics are fetched out of order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Boundary {
    pub start_block_hash: Hash32,
    pub end_block_hash: Hash32,
    pub parent_hash_of_start: Hash32,
}

/// One file in the relic.
///
/// Two hashes, deliberately. `blake3` is over the bytes of *this copy* and catches a tampered or
/// truncated object. `content_hash` is over the canonical row encoding and is framing-independent,
/// so two Shadows built against different arrow-rs versions can still be held to agreement
/// (§11.1). Byte-identity is the aspiration; content-identity is the conformance bar.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub table: Table,
    pub byte_size: u64,
    pub blake3: Hash32,
    pub content_hash: Hash32,
    pub row_count: u64,
    pub row_groups: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SignatureScheme {
    #[serde(rename = "ethereum-secp256k1")]
    EthereumSecp256k1,
    #[serde(rename = "ed25519")]
    Ed25519,
}

/// An optional producer attestation over the manifest hash. Optional on purpose: the corpus is
/// content-addressed, so a signature adds accountability, not authority. A mirror that cannot
/// identify the producer can still verify every byte.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Producer {
    pub identity: String,
    pub scheme: SignatureScheme,
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub spec_version: u32,
    pub chain_id: u64,
    pub silo: String,
    pub block_range: BlockRange,
    pub boundary: Boundary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub era1_accumulator_root: Option<Hash32>,
    pub blocks_per_relic: u64,
    pub files: Vec<FileEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer: Option<Producer>,
    pub prev_relic_manifest_hash: Hash32,
    pub pact_root: Hash32,
    pub hash_algo: String,
    /// Declares that this relic starts a pact segment with no sealed predecessor (§8.4.1). Absent
    /// when false, so manifests sealed before the field existed hash exactly as they did.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached_pact_start: bool,
}

impl Manifest {
    /// A manifest for a relic with nothing in it yet. `pact_root` and `prev_relic_manifest_hash`
    /// are left zero; `crate::pact::seal_chain` fills them in.
    pub fn new(chain_id: u64, silo: impl Into<String>, index: u64, boundary: Boundary) -> Self {
        Manifest {
            spec_version: SPEC_VERSION,
            chain_id,
            silo: silo.into(),
            block_range: relic_range(index),
            boundary,
            era1_accumulator_root: None,
            blocks_per_relic: BLOCKS_PER_RELIC,
            files: Vec::new(),
            producer: None,
            prev_relic_manifest_hash: Hash32::ZERO,
            pact_root: Hash32::ZERO,
            hash_algo: HASH_ALGO.to_string(),
            detached_pact_start: false,
        }
    }

    /// The relic's index, derived rather than stored: the range is the truth, and two fields that
    /// can disagree are one field too many.
    pub fn relic_index(&self) -> u64 {
        relic_index(self.block_range.start)
    }

    /// Canonical (JCS) bytes of this manifest, exactly as published.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, Error> {
        Ok(jcs::to_canonical_bytes(self)?)
    }

    /// The manifest hash: BLAKE3 over the JCS bytes **with `pact_root` zeroed** (§8.4).
    ///
    /// Zeroing is what makes the construction work at all. The pact root is computed *from* this
    /// hash and then written back into the document, so hashing the document as published would
    /// require the hash of a field that depends on the hash.
    pub fn manifest_hash(&self) -> Result<Hash32, Error> {
        let mut bare = self.clone();
        bare.pact_root = Hash32::ZERO;
        Ok(blake3(&bare.to_canonical_bytes()?))
    }

    /// Structural checks that need nothing but the manifest itself. This is the cheap half of
    /// cleaning; the expensive half rebuilds tries and is not in this crate.
    pub fn validate(&self) -> Result<(), Error> {
        let bad = |m: String| Err(Error::Manifest(m));

        if self.hash_algo != HASH_ALGO {
            return bad(format!(
                "hash_algo is {:?}, this build only knows {HASH_ALGO:?}",
                self.hash_algo
            ));
        }
        if self.blocks_per_relic != BLOCKS_PER_RELIC {
            return bad(format!(
                "blocks_per_relic is {}, this build's geometry is {BLOCKS_PER_RELIC}",
                self.blocks_per_relic
            ));
        }
        if self.block_range.end < self.block_range.start {
            return bad(format!("block_range {} runs backwards", self.block_range));
        }
        let range_len = (self.block_range.end - self.block_range.start).checked_add(1);
        if range_len != Some(self.blocks_per_relic) {
            return bad(format!(
                "block_range {} does not cover exactly {} blocks",
                self.block_range, self.blocks_per_relic
            ));
        }
        if !self.block_range.start.is_multiple_of(self.blocks_per_relic) {
            return bad(format!(
                "block_range {} does not start on a relic boundary",
                self.block_range
            ));
        }
        if self.files.is_empty() {
            return bad("a relic with no files is not a relic".into());
        }
        if self.detached_pact_start && self.relic_index() == 0 {
            return bad(
                "the genesis relic is anchored by definition and cannot be detached".into(),
            );
        }

        for file in &self.files {
            if file.name != file.table.file_name() {
                return bad(format!(
                    "file name {:?} must be {:?} for {:?}",
                    file.name,
                    file.table.file_name(),
                    file.table
                ));
            }
        }

        let mut names: Vec<&str> = self.files.iter().map(|f| f.name.as_str()).collect();
        names.sort_unstable();
        if names.windows(2).any(|w| w[0] == w[1]) {
            return bad("two files in one manifest share a name".into());
        }

        // headers is what every other table is verified against, so its absence is not a
        // stylistic choice.
        if !self.files.iter().any(|f| f.table == Table::Headers) {
            return bad("no headers table; nothing in this relic can be verified".into());
        }
        Ok(())
    }

    /// Whether any file in this relic is outside the reach of header-committed verification.
    pub fn has_unverifiable_tier(&self) -> bool {
        self.files.iter().any(|f| !f.table.is_header_committed())
    }
}

#[cfg(test)]
pub(crate) fn sample_manifest(index: u64) -> Manifest {
    use crate::hash::blake3 as h;

    let mut m = Manifest::new(
        1,
        "Silo 1",
        index,
        Boundary {
            start_block_hash: h(format!("start{index}").as_bytes()),
            end_block_hash: h(format!("end{index}").as_bytes()),
            parent_hash_of_start: h(format!("end{}", index.wrapping_sub(1)).as_bytes()),
        },
    );
    for table in [Table::Headers, Table::Logs] {
        m.files.push(FileEntry {
            name: table.file_name().to_string(),
            table,
            byte_size: 1024 * (index + 1),
            blake3: h(format!("{index}/{}", table.file_name()).as_bytes()),
            content_hash: h(format!("{index}/{}/rows", table.file_name()).as_bytes()),
            row_count: 8192,
            row_groups: 1,
        });
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_manifest_validates() {
        sample_manifest(2451).validate().unwrap();
    }

    #[test]
    fn the_hash_ignores_the_pact_root_and_nothing_else() {
        let m = sample_manifest(7);
        let before = m.manifest_hash().unwrap();

        let mut with_root = m.clone();
        with_root.pact_root = blake3(b"anything at all");
        assert_eq!(with_root.manifest_hash().unwrap(), before);

        let mut with_a_changed_file = m.clone();
        with_a_changed_file.files[0].byte_size += 1;
        assert_ne!(with_a_changed_file.manifest_hash().unwrap(), before);
    }

    #[test]
    fn the_hash_survives_a_json_round_trip() {
        let m = sample_manifest(3);
        let text = String::from_utf8(m.to_canonical_bytes().unwrap()).unwrap();
        let parsed: Manifest = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, m);
        assert_eq!(parsed.manifest_hash().unwrap(), m.manifest_hash().unwrap());
    }

    #[test]
    fn canonical_output_is_sorted_and_unspaced() {
        let text = String::from_utf8(sample_manifest(0).to_canonical_bytes().unwrap()).unwrap();
        assert!(
            text.starts_with(r#"{"block_range":{"end":8191,"start":0},"blocks_per_relic":8192,"#)
        );
        // No insignificant whitespace. The space inside `"Silo 1"` is significant and stays, which
        // is why this looks for the separators rather than for spaces.
        assert!(!text.contains(": "));
        assert!(!text.contains(", "));
        assert!(!text.contains('\n'));
    }

    #[test]
    fn an_absent_optional_field_is_absent_rather_than_null() {
        let text = String::from_utf8(sample_manifest(0).to_canonical_bytes().unwrap()).unwrap();
        assert!(!text.contains("era1_accumulator_root"));
        assert!(!text.contains("producer"));
    }

    #[test]
    fn validation_catches_the_ways_a_manifest_goes_wrong() {
        type Breakage = (&'static str, Box<dyn Fn(&mut Manifest)>);
        let cases: Vec<Breakage> = vec![
            (
                "wrong geometry",
                Box::new(|m: &mut Manifest| m.blocks_per_relic = 10_000),
            ),
            (
                "wrong hash algo",
                Box::new(|m: &mut Manifest| m.hash_algo = "sha256".into()),
            ),
            (
                "short range",
                Box::new(|m: &mut Manifest| m.block_range.end -= 1),
            ),
            (
                "unaligned start",
                Box::new(|m: &mut Manifest| {
                    m.block_range.start += 1;
                    m.block_range.end += 1;
                }),
            ),
            ("no files", Box::new(|m: &mut Manifest| m.files.clear())),
            (
                "overflowing range",
                Box::new(|m: &mut Manifest| {
                    m.block_range.start = 0;
                    m.block_range.end = u64::MAX;
                }),
            ),
            (
                "path traversal",
                Box::new(|m: &mut Manifest| m.files[0].name = "../headers.parquet".into()),
            ),
            (
                "absolute file name",
                Box::new(|m: &mut Manifest| m.files[0].name = "/headers.parquet".into()),
            ),
            (
                "table/name mismatch",
                Box::new(|m: &mut Manifest| m.files[0].name = "transactions.parquet".into()),
            ),
            (
                "no headers",
                Box::new(|m: &mut Manifest| m.files.retain(|f| f.table != Table::Headers)),
            ),
            (
                "duplicate names",
                Box::new(|m: &mut Manifest| {
                    let dup = m.files[0].clone();
                    m.files.push(dup);
                }),
            ),
        ];
        for (what, break_it) in cases {
            let mut m = sample_manifest(1);
            break_it(&mut m);
            assert!(m.validate().is_err(), "{what} should not have validated");
        }
    }

    #[test]
    fn the_traces_tier_is_flagged_as_unverifiable() {
        let mut m = sample_manifest(1);
        assert!(!m.has_unverifiable_tier());
        m.files.push(FileEntry {
            name: Table::Traces.file_name().to_string(),
            table: Table::Traces,
            byte_size: 1,
            blake3: Hash32::ZERO,
            content_hash: Hash32::ZERO,
            row_count: 0,
            row_groups: 0,
        });
        assert!(m.has_unverifiable_tier());
    }
}
