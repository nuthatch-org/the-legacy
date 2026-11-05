//! JSON-RPC block, transaction, receipt and withdrawal objects as nodes serve them.
//!
//! Everything stays a string until it is parsed by a helper below, so a provider that encodes a
//! quantity with leading zeros or omits a field fails with the field's name rather than being
//! silently coerced by serde.

use serde::{Deserialize, Serialize};

use super::Error;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    pub number: String,
    pub hash: String,
    pub parent_hash: String,
    pub sha3_uncles: String,
    pub miner: String,
    pub state_root: String,
    pub transactions_root: String,
    pub receipts_root: String,
    pub logs_bloom: String,
    pub difficulty: String,
    pub gas_limit: String,
    pub gas_used: String,
    pub timestamp: String,
    pub extra_data: String,
    pub mix_hash: String,
    pub nonce: String,
    #[serde(default)]
    pub base_fee_per_gas: Option<String>,
    #[serde(default)]
    pub withdrawals_root: Option<String>,
    #[serde(default)]
    pub blob_gas_used: Option<String>,
    #[serde(default)]
    pub excess_blob_gas: Option<String>,
    #[serde(default)]
    pub parent_beacon_block_root: Option<String>,
    #[serde(default)]
    pub requests_hash: Option<String>,
    #[serde(default)]
    pub total_difficulty: Option<String>,
    #[serde(default)]
    pub uncles: Vec<String>,
    pub transactions: Vec<Transaction>,
    #[serde(default)]
    pub withdrawals: Option<Vec<Withdrawal>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    pub hash: String,
    #[serde(rename = "type", default)]
    pub tx_type: Option<String>,
    pub nonce: String,
    pub from: String,
    #[serde(default)]
    pub to: Option<String>,
    pub value: String,
    pub gas: String,
    pub input: String,
    #[serde(default)]
    pub gas_price: Option<String>,
    #[serde(default)]
    pub max_fee_per_gas: Option<String>,
    #[serde(default)]
    pub max_priority_fee_per_gas: Option<String>,
    #[serde(default)]
    pub max_fee_per_blob_gas: Option<String>,
    #[serde(default)]
    pub access_list: Option<Vec<AccessItem>>,
    #[serde(default)]
    pub blob_versioned_hashes: Option<Vec<String>>,
    #[serde(default)]
    pub authorization_list: Option<Vec<Authorization>>,
    #[serde(default)]
    pub chain_id: Option<String>,
    #[serde(default)]
    pub v: Option<String>,
    #[serde(default)]
    pub y_parity: Option<String>,
    pub r: String,
    pub s: String,
    #[serde(default)]
    pub transaction_index: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessItem {
    pub address: String,
    pub storage_keys: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Authorization {
    pub chain_id: String,
    pub address: String,
    pub nonce: String,
    #[serde(default)]
    pub y_parity: Option<String>,
    #[serde(default)]
    pub v: Option<String>,
    pub r: String,
    pub s: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Withdrawal {
    pub index: String,
    pub validator_index: String,
    pub address: String,
    pub amount: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub transaction_hash: String,
    pub transaction_index: String,
    #[serde(rename = "type", default)]
    pub tx_type: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub root: Option<String>,
    pub cumulative_gas_used: String,
    pub logs_bloom: String,
    pub logs: Vec<Log>,
    #[serde(default)]
    pub gas_used: Option<String>,
    #[serde(default)]
    pub contract_address: Option<String>,
    #[serde(default)]
    pub effective_gas_price: Option<String>,
    #[serde(default)]
    pub blob_gas_used: Option<String>,
    #[serde(default)]
    pub blob_gas_price: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Log {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
    #[serde(default)]
    pub log_index: Option<String>,
    #[serde(default)]
    pub removed: Option<bool>,
}

/// One block as the journal holds it: the two responses, untouched.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Journaled {
    pub block: Block,
    pub receipts: Vec<Receipt>,
}

fn strip<'a>(field: &str, text: &'a str) -> Result<&'a str, Error> {
    text.strip_prefix("0x")
        .ok_or_else(|| Error::Wire(format!("{field} is not 0x-prefixed: {text:?}")))
}

/// `DATA`: any even-length hex string.
pub fn data(field: &str, text: &str) -> Result<Vec<u8>, Error> {
    hex::decode(strip(field, text)?).map_err(|e| Error::Wire(format!("{field}: {e}")))
}

pub fn fixed<const N: usize>(field: &str, text: &str) -> Result<[u8; N], Error> {
    let bytes = data(field, text)?;
    bytes
        .try_into()
        .map_err(|b: Vec<u8>| Error::Wire(format!("{field} is {} bytes, not {N}", b.len())))
}

/// `QUANTITY` as a minimal big-endian magnitude, zero as empty. Leading zeros are refused, as the
/// JSON-RPC spec requires, except for signature scalars which some nodes zero-pad to 32 bytes.
pub fn quantity(field: &str, text: &str) -> Result<Vec<u8>, Error> {
    let digits = strip(field, text)?;
    if digits.is_empty() {
        return Err(Error::Wire(format!("{field} is an empty quantity")));
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return Err(Error::Wire(format!("{field} has leading zeros: {text:?}")));
    }
    if digits == "0" {
        return Ok(Vec::new());
    }
    let padded = if digits.len() % 2 == 1 {
        format!("0{digits}")
    } else {
        digits.to_string()
    };
    let bytes = hex::decode(padded).map_err(|e| Error::Wire(format!("{field}: {e}")))?;
    if bytes.len() > 32 {
        return Err(Error::Wire(format!("{field} is wider than uint256")));
    }
    Ok(bytes)
}

/// A signature scalar, accepted as a quantity or as zero-padded 32-byte data.
pub fn scalar(field: &str, text: &str) -> Result<Vec<u8>, Error> {
    let digits = strip(field, text)?;
    let padded = if digits.len() % 2 == 1 {
        format!("0{digits}")
    } else {
        digits.to_string()
    };
    let bytes = hex::decode(padded).map_err(|e| Error::Wire(format!("{field}: {e}")))?;
    if bytes.len() > 32 {
        return Err(Error::Wire(format!("{field} is wider than 32 bytes")));
    }
    let start = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len());
    Ok(bytes[start..].to_vec())
}

pub fn u64_of(field: &str, text: &str) -> Result<u64, Error> {
    let bytes = quantity(field, text)?;
    if bytes.len() > 8 {
        return Err(Error::Wire(format!("{field} does not fit in u64")));
    }
    let mut wide = [0u8; 8];
    wide[8 - bytes.len()..].copy_from_slice(&bytes);
    Ok(u64::from_be_bytes(wide))
}

pub fn opt_u64(field: &str, text: &Option<String>) -> Result<Option<u64>, Error> {
    text.as_deref().map(|t| u64_of(field, t)).transpose()
}

pub fn opt_quantity(field: &str, text: &Option<String>) -> Result<Option<Vec<u8>>, Error> {
    text.as_deref().map(|t| quantity(field, t)).transpose()
}

pub fn hex_u64(value: u64) -> String {
    format!("0x{value:x}")
}
