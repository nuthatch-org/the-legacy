//! One journaled block in, canonical rows out.
//!
//! The JSON fields are re-encoded into the consensus RLP the header and tries commit to, then
//! decoded by the same code era1 uses. Fields a node serves that are not committed (sender, gas
//! used, contract address, effective gas price, log index) are compared with what that decoder
//! derives, so a disagreement stops the relic instead of picking a side.

use alloy_rlp::Encodable;
use legacy_format::headers::HeaderRow;
use legacy_format::logs::LogRow;
use legacy_format::receipts::ReceiptRow;
use legacy_format::transactions::TransactionRow;
use legacy_format::withdrawals::WithdrawalRow;

use super::wire::{self, Journaled};
use super::Error;
use crate::decode::{self, list_bytes};

#[derive(Debug)]
pub struct Rows {
    pub header: HeaderRow,
    pub transactions: Vec<TransactionRow>,
    pub receipts: Vec<ReceiptRow>,
    pub logs: Vec<LogRow>,
    /// `None` before Shanghai: the block has no withdrawals list at all.
    pub withdrawals: Option<Vec<WithdrawalRow>>,
}

/// Chain ID 1 Byzantium activation; receipts before it carry a state root, not a status.
const BYZANTIUM: u64 = 4_370_000;

const EMPTY_OMMERS: [u8; 32] = [
    0x1d, 0xcc, 0x4d, 0xe8, 0xde, 0xc7, 0x5d, 0x7a, 0xab, 0x85, 0xb5, 0x67, 0xb6, 0xcc, 0xd4, 0x1a,
    0xd3, 0x12, 0x45, 0x1b, 0x94, 0x8a, 0x74, 0x13, 0xf0, 0xa1, 0x42, 0xfd, 0x40, 0xd4, 0x93, 0x47,
];

/// Decode and check one block. `number` is the block that was asked for.
pub fn rows(number: u64, journaled: &Journaled) -> Result<Rows, Error> {
    let block = &journaled.block;
    let fail = |reason: String| Error::Block { number, reason };

    let header = header_row(number, block)?;
    if block.uncles.is_empty() != (header.ommers_hash == EMPTY_OMMERS) {
        return Err(fail(
            "uncle list and ommers hash disagree on whether there are uncles".into(),
        ));
    }

    let mut items = Vec::with_capacity(block.transactions.len());
    for (position, tx) in block.transactions.iter().enumerate() {
        if let Some(index) = &tx.transaction_index {
            if wire::u64_of("transactionIndex", index)? != position as u64 {
                return Err(fail(format!(
                    "transaction {position} is served out of order"
                )));
            }
        }
        let envelope = envelope(tx).map_err(|e| fail(format!("transaction {position}: {e}")))?;
        // A block body lists legacy transactions as RLP lists and typed ones as RLP strings.
        items.push(if envelope[0] >= 0xc0 {
            envelope
        } else {
            let mut wrapped = Vec::new();
            envelope.as_slice().encode(&mut wrapped);
            wrapped
        });
    }

    let receipts = &journaled.receipts;
    // Before Byzantium a receipt commits to a 32-byte post-state root. Nodes that no longer keep
    // it serve a `status` instead, and no amount of retrying turns that into the committed bytes.
    if number < BYZANTIUM && receipts.iter().any(|r| r.root.is_none()) {
        return Err(Error::Probe(format!(
            "block {number} is pre-Byzantium and the provider served receipts without the \
             post-state `root`; it cannot reproduce chain ID 1 receipts before block {BYZANTIUM}"
        )));
    }
    if receipts.len() != block.transactions.len() {
        return Err(fail(format!(
            "{} receipts for {} transactions; an empty or short receipt list is a provider error",
            receipts.len(),
            block.transactions.len()
        )));
    }
    let mut receipt_items = Vec::with_capacity(receipts.len());
    for (position, receipt) in receipts.iter().enumerate() {
        receipt_items
            .push(receipt_rlp(receipt).map_err(|e| fail(format!("receipt {position}: {e}")))?);
    }
    let refs: Vec<&[u8]> = receipt_items.iter().map(Vec::as_slice).collect();
    let item_refs: Vec<&[u8]> = items.iter().map(Vec::as_slice).collect();
    let decoded = decode::decode_payload(header, &item_refs, &list_bytes(&refs))
        .map_err(|e| fail(e.to_string()))?;

    let mut receipts_out = decoded.receipts;
    for (position, (tx, served)) in decoded
        .transactions
        .iter()
        .zip(&block.transactions)
        .enumerate()
    {
        if tx.transaction_hash != wire::fixed::<32>("hash", &served.hash)? {
            return Err(fail(format!(
                "transaction {position}: rebuilt envelope does not hash to the served hash"
            )));
        }
        if tx.from != wire::fixed::<20>("from", &served.from)? {
            return Err(fail(format!(
                "transaction {position}: recovered sender differs from the served `from`"
            )));
        }
    }
    for (position, (row, served)) in receipts_out.iter_mut().zip(receipts).enumerate() {
        let what = |field: &str| fail(format!("receipt {position}: served {field} disagrees"));
        if wire::fixed::<32>("transactionHash", &served.transaction_hash)? != row.transaction_hash
            || wire::u64_of("transactionIndex", &served.transaction_index)?
                != u64::from(row.transaction_index)
        {
            return Err(what("transaction hash or index"));
        }
        if let Some(gas) = &served.gas_used {
            if Some(wire::u64_of("gasUsed", gas)?) != row.gas_used {
                return Err(what("gasUsed"));
            }
        }
        let served_contract = match served.contract_address.as_deref() {
            None => None,
            Some(text) => Some(wire::fixed::<20>("contractAddress", text)?),
        };
        if served_contract != row.contract_address {
            return Err(what("contractAddress"));
        }
        if let Some(price) = &served.effective_gas_price {
            if Some(wire::quantity("effectiveGasPrice", price)?) != row.effective_gas_price {
                return Err(what("effectiveGasPrice"));
            }
        }
        if row.tx_type == 3 {
            // Blob gas is a derived receipt field the trie does not commit; it is kept as served.
            row.blob_gas_used = wire::opt_u64("blobGasUsed", &served.blob_gas_used)?;
            row.blob_gas_price = wire::opt_quantity("blobGasPrice", &served.blob_gas_price)?;
        }
    }

    let served_logs = receipts.iter().flat_map(|r| r.logs.iter());
    for (row, served) in decoded.logs.iter().zip(served_logs) {
        if served.removed == Some(true) {
            return Err(fail(format!("log {} is marked removed", row.log_index)));
        }
        if let Some(index) = &served.log_index {
            if wire::u64_of("logIndex", index)? != u64::from(row.log_index) {
                return Err(fail(format!(
                    "log {} has served logIndex {index}",
                    row.log_index
                )));
            }
        }
    }

    let withdrawals = match (&decoded.header.withdrawals_root, &block.withdrawals) {
        (None, None) => None,
        (Some(_), Some(list)) => Some(
            list.iter()
                .map(|w| {
                    Ok(WithdrawalRow {
                        block_number: number,
                        index: wire::u64_of("withdrawal.index", &w.index)?,
                        validator_index: wire::u64_of("validatorIndex", &w.validator_index)?,
                        address: wire::fixed("withdrawal.address", &w.address)?,
                        amount: wire::u64_of("withdrawal.amount", &w.amount)?,
                    })
                })
                .collect::<Result<Vec<_>, Error>>()?,
        ),
        _ => {
            return Err(fail(
                "withdrawals list and withdrawalsRoot disagree on presence".into(),
            ))
        }
    };

    let rows = Rows {
        header: decoded.header,
        transactions: decoded.transactions,
        receipts: receipts_out,
        logs: decoded.logs,
        withdrawals,
    };
    verify(&rows).map_err(|e| fail(e.to_string()))?;
    Ok(rows)
}

/// The checks `solo clean --files` repeats later, run on one block before it is journaled so a
/// provider serving a wrong body is caught before its neighbours are paid for.
fn verify(rows: &Rows) -> Result<(), Box<dyn std::error::Error>> {
    let headers = std::slice::from_ref(&rows.header);
    legacy_format::ethereum::verify_header(&rows.header)?;
    legacy_format::ethereum_transactions::verify_transaction_roots(headers, &rows.transactions)?;
    legacy_format::ethereum_receipts::verify_receipt_roots(headers, &rows.receipts, &rows.logs)?;
    legacy_format::bloom::receipt_blooms(&rows.logs, &rows.receipts)?;
    legacy_format::bloom::header_blooms(headers, &rows.receipts)?;
    legacy_format::consistency::ethereum_receipt_gas(headers, &rows.receipts)?;
    if let Some(withdrawals) = &rows.withdrawals {
        legacy_format::ethereum_withdrawals::verify_withdrawal_roots(headers, withdrawals)?;
    }
    Ok(())
}

fn header_row(number: u64, block: &wire::Block) -> Result<HeaderRow, Error> {
    let served_number = wire::u64_of("number", &block.number)?;
    if served_number != number {
        return Err(Error::Block {
            number,
            reason: format!("provider returned block {served_number}"),
        });
    }
    let difficulty = wire::quantity("difficulty", &block.difficulty)?;
    // Post-merge total difficulty is frozen and many nodes no longer serve it; RFC-0004 §8.1
    // stores null. Pre-merge a served value is kept; an absent one stays None here and the seal
    // either derives it from --total-difficulty-base or refuses (`fill_total_difficulty`).
    let total_difficulty = if difficulty.is_empty() {
        None
    } else {
        block
            .total_difficulty
            .as_deref()
            .map(|t| wire::quantity("totalDifficulty", t))
            .transpose()?
    };
    let mut row = HeaderRow {
        block_number: number,
        block_hash: wire::fixed("hash", &block.hash)?,
        parent_hash: wire::fixed("parentHash", &block.parent_hash)?,
        ommers_hash: wire::fixed("sha3Uncles", &block.sha3_uncles)?,
        beneficiary: wire::fixed("miner", &block.miner)?,
        state_root: wire::fixed("stateRoot", &block.state_root)?,
        transactions_root: wire::fixed("transactionsRoot", &block.transactions_root)?,
        receipts_root: wire::fixed("receiptsRoot", &block.receipts_root)?,
        logs_bloom: wire::fixed("logsBloom", &block.logs_bloom)?,
        difficulty,
        gas_limit: wire::u64_of("gasLimit", &block.gas_limit)?,
        gas_used: wire::u64_of("gasUsed", &block.gas_used)?,
        timestamp: wire::u64_of("timestamp", &block.timestamp)?,
        extra_data: wire::data("extraData", &block.extra_data)?,
        mix_hash: wire::fixed("mixHash", &block.mix_hash)?,
        nonce: wire::fixed("nonce", &block.nonce)?,
        base_fee_per_gas: wire::opt_quantity("baseFeePerGas", &block.base_fee_per_gas)?,
        withdrawals_root: block
            .withdrawals_root
            .as_deref()
            .map(|t| wire::fixed("withdrawalsRoot", t))
            .transpose()?,
        blob_gas_used: wire::opt_u64("blobGasUsed", &block.blob_gas_used)?,
        excess_blob_gas: wire::opt_u64("excessBlobGas", &block.excess_blob_gas)?,
        parent_beacon_block_root: block
            .parent_beacon_block_root
            .as_deref()
            .map(|t| wire::fixed("parentBeaconBlockRoot", t))
            .transpose()?,
        requests_hash: block
            .requests_hash
            .as_deref()
            .map(|t| wire::fixed("requestsHash", t))
            .transpose()?,
        total_difficulty,
    };
    let rebuilt = legacy_format::ethereum::header_hash(&row).map_err(|e| Error::Block {
        number,
        reason: e.to_string(),
    })?;
    if rebuilt != row.block_hash {
        return Err(Error::Block {
            number,
            reason: "header fields do not hash to the served block hash".into(),
        });
    }
    row.block_hash = rebuilt;
    Ok(row)
}

fn rlp<T: Encodable + ?Sized>(value: &T) -> Vec<u8> {
    let mut out = Vec::new();
    value.encode(&mut out);
    out
}

fn bytes_item(bytes: &[u8]) -> Vec<u8> {
    rlp(bytes)
}

fn quantity_item(field: &str, text: &str) -> Result<Vec<u8>, Error> {
    Ok(bytes_item(&wire::quantity(field, text)?))
}

fn required<'a>(field: &str, value: &'a Option<String>) -> Result<&'a str, Error> {
    value
        .as_deref()
        .ok_or_else(|| Error::Wire(format!("missing {field}")))
}

fn access_list(items: &[wire::AccessItem]) -> Result<Vec<u8>, Error> {
    let mut entries = Vec::with_capacity(items.len());
    for item in items {
        let address = bytes_item(&wire::fixed::<20>("accessList.address", &item.address)?);
        let keys: Vec<Vec<u8>> = item
            .storage_keys
            .iter()
            .map(|k| Ok(bytes_item(&wire::fixed::<32>("storageKeys", k)?)))
            .collect::<Result<_, Error>>()?;
        let key_refs: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
        let keys = list_bytes(&key_refs);
        entries.push(list_bytes(&[&address, &keys]));
    }
    let refs: Vec<&[u8]> = entries.iter().map(Vec::as_slice).collect();
    Ok(list_bytes(&refs))
}

/// `yParity` where served, else `v`, which older nodes use for typed transactions too.
fn y_parity(y_parity: &Option<String>, v: &Option<String>) -> Result<Vec<u8>, Error> {
    let text = y_parity
        .as_deref()
        .or(v.as_deref())
        .ok_or_else(|| Error::Wire("missing yParity and v".into()))?;
    let value = wire::u64_of("yParity", text)?;
    if value > 1 {
        return Err(Error::Wire(format!("typed yParity is {value}")));
    }
    Ok(rlp(&value))
}

/// Rebuild the EIP-2718 envelope a transaction hash commits to. Unknown types are refused.
pub fn envelope(tx: &wire::Transaction) -> Result<Vec<u8>, Error> {
    let tx_type = match &tx.tx_type {
        None => 0,
        Some(text) => wire::u64_of("type", text)?,
    };
    let to = match tx.to.as_deref() {
        None => Vec::new(),
        Some(text) => wire::fixed::<20>("to", text)?.to_vec(),
    };
    let nonce = quantity_item("nonce", &tx.nonce)?;
    let gas = quantity_item("gas", &tx.gas)?;
    let to = bytes_item(&to);
    let value = quantity_item("value", &tx.value)?;
    let input = bytes_item(&wire::data("input", &tx.input)?);
    let r = bytes_item(&wire::scalar("r", &tx.r)?);
    let s = bytes_item(&wire::scalar("s", &tx.s)?);

    if tx_type == 0 {
        let gas_price = quantity_item("gasPrice", required("gasPrice", &tx.gas_price)?)?;
        let v = quantity_item("v", required("v", &tx.v)?)?;
        return Ok(list_bytes(&[
            &nonce, &gas_price, &gas, &to, &value, &input, &v, &r, &s,
        ]));
    }

    let chain_id = quantity_item("chainId", required("chainId", &tx.chain_id)?)?;
    let access = access_list(tx.access_list.as_deref().unwrap_or_default())?;
    let parity = y_parity(&tx.y_parity, &tx.v)?;
    let fields: Vec<Vec<u8>> = match tx_type {
        1 => vec![
            chain_id,
            nonce,
            quantity_item("gasPrice", required("gasPrice", &tx.gas_price)?)?,
            gas,
            to,
            value,
            input,
            access,
        ],
        2..=4 => {
            let mut fields = vec![
                chain_id,
                nonce,
                quantity_item(
                    "maxPriorityFeePerGas",
                    required("maxPriorityFeePerGas", &tx.max_priority_fee_per_gas)?,
                )?,
                quantity_item(
                    "maxFeePerGas",
                    required("maxFeePerGas", &tx.max_fee_per_gas)?,
                )?,
                gas,
                to,
                value,
                input,
                access,
            ];
            if tx_type == 3 {
                fields.push(quantity_item(
                    "maxFeePerBlobGas",
                    required("maxFeePerBlobGas", &tx.max_fee_per_blob_gas)?,
                )?);
                let hashes: Vec<Vec<u8>> = tx
                    .blob_versioned_hashes
                    .as_deref()
                    .ok_or_else(|| Error::Wire("missing blobVersionedHashes".into()))?
                    .iter()
                    .map(|h| Ok(bytes_item(&wire::fixed::<32>("blobVersionedHashes", h)?)))
                    .collect::<Result<_, Error>>()?;
                let refs: Vec<&[u8]> = hashes.iter().map(Vec::as_slice).collect();
                fields.push(list_bytes(&refs));
            }
            if tx_type == 4 {
                let mut tuples = Vec::new();
                for auth in tx
                    .authorization_list
                    .as_deref()
                    .ok_or_else(|| Error::Wire("missing authorizationList".into()))?
                {
                    tuples.push(list_bytes(&[
                        &quantity_item("authorization.chainId", &auth.chain_id)?,
                        &bytes_item(&wire::fixed::<20>("authorization.address", &auth.address)?),
                        &quantity_item("authorization.nonce", &auth.nonce)?,
                        &y_parity(&auth.y_parity, &auth.v)?,
                        &bytes_item(&wire::scalar("authorization.r", &auth.r)?),
                        &bytes_item(&wire::scalar("authorization.s", &auth.s)?),
                    ]));
                }
                let refs: Vec<&[u8]> = tuples.iter().map(Vec::as_slice).collect();
                fields.push(list_bytes(&refs));
            }
            fields
        }
        other => {
            return Err(Error::Wire(format!(
                "transaction type {other} is not supported; the relic stops here"
            )))
        }
    };
    let mut refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
    refs.extend([parity.as_slice(), &r, &s]);
    let mut out = vec![tx_type as u8];
    out.extend(list_bytes(&refs));
    Ok(out)
}

/// Rebuild one receipt as a block's receipt list carries it: legacy as a list, typed as a string
/// wrapping `type || rlp(...)`.
fn receipt_rlp(receipt: &wire::Receipt) -> Result<Vec<u8>, Error> {
    let tx_type = match &receipt.tx_type {
        None => 0,
        Some(text) => wire::u64_of("type", text)?,
    };
    let outcome = match (&receipt.status, &receipt.root) {
        (_, Some(root)) => bytes_item(&wire::fixed::<32>("root", root)?),
        (Some(status), None) => quantity_item("status", status)?,
        (None, None) => return Err(Error::Wire("receipt has neither status nor root".into())),
    };
    let cumulative = quantity_item("cumulativeGasUsed", &receipt.cumulative_gas_used)?;
    let bloom = bytes_item(&wire::fixed::<256>("logsBloom", &receipt.logs_bloom)?);
    let mut logs = Vec::with_capacity(receipt.logs.len());
    for log in &receipt.logs {
        let address = bytes_item(&wire::fixed::<20>("log.address", &log.address)?);
        let topics: Vec<Vec<u8>> = log
            .topics
            .iter()
            .map(|t| Ok(bytes_item(&wire::fixed::<32>("topic", t)?)))
            .collect::<Result<_, Error>>()?;
        let topic_refs: Vec<&[u8]> = topics.iter().map(Vec::as_slice).collect();
        let topics = list_bytes(&topic_refs);
        let data = bytes_item(&wire::data("log.data", &log.data)?);
        logs.push(list_bytes(&[&address, &topics, &data]));
    }
    let log_refs: Vec<&[u8]> = logs.iter().map(Vec::as_slice).collect();
    let body = list_bytes(&[&outcome, &cumulative, &bloom, &list_bytes(&log_refs)]);
    if tx_type == 0 {
        return Ok(body);
    }
    let mut typed = vec![tx_type as u8];
    typed.extend(body);
    Ok(bytes_item(&typed))
}

/// Fill pre-merge total difficulty a provider did not serve, as the running sum of each header's
/// own difficulty from `base`, the total difficulty of the block before the relic. Difficulties
/// are header-committed; the base is not, and whoever supplies it is recorded in provenance.
/// A served value must agree with the running sum, so a provider cannot be half right.
pub fn fill_total_difficulty(
    headers: &mut [HeaderRow],
    base: Option<&[u8]>,
) -> Result<bool, Error> {
    let mut running: Option<Vec<u8>> = base.map(<[u8]>::to_vec);
    let mut derived = false;
    for header in headers.iter_mut() {
        if header.difficulty.is_empty() {
            running = None;
            continue;
        }
        let next = running
            .as_deref()
            .map(|r| crate::decode::be_add_pub(r, &header.difficulty))
            .transpose()
            .map_err(|_| Error::Block {
                number: header.block_number,
                reason: "total difficulty overflowed uint256".into(),
            })?;
        match (&header.total_difficulty, next) {
            (Some(served), Some(sum)) if *served != sum => {
                return Err(Error::Block {
                    number: header.block_number,
                    reason: "served totalDifficulty disagrees with the running sum from the base"
                        .into(),
                })
            }
            (Some(served), _) => running = Some(served.clone()),
            (None, Some(sum)) => {
                header.total_difficulty = Some(sum.clone());
                running = Some(sum);
                derived = true;
            }
            (None, None) => {
                return Err(Error::Block {
                    number: header.block_number,
                    reason: "pre-merge block served without totalDifficulty; pass \
                             --total-difficulty-base (the total difficulty of the block before \
                             the relic) to derive it"
                        .into(),
                })
            }
        }
    }
    Ok(derived)
}

#[cfg(test)]
mod td_tests {
    use super::*;

    fn header(n: u64, difficulty: u64, td: Option<u64>) -> HeaderRow {
        let trim = |v: u64| {
            v.to_be_bytes()
                .iter()
                .copied()
                .skip_while(|b| *b == 0)
                .collect::<Vec<u8>>()
        };
        HeaderRow {
            block_number: n,
            block_hash: [0; 32],
            parent_hash: [0; 32],
            ommers_hash: [0; 32],
            beneficiary: [0; 20],
            state_root: [0; 32],
            transactions_root: [0; 32],
            receipts_root: [0; 32],
            logs_bloom: [0; 256],
            difficulty: trim(difficulty),
            gas_limit: 0,
            gas_used: 0,
            timestamp: 0,
            extra_data: vec![],
            mix_hash: [0; 32],
            nonce: [0; 8],
            base_fee_per_gas: None,
            withdrawals_root: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
            requests_hash: None,
            total_difficulty: td.map(trim),
        }
    }

    #[test]
    fn a_missing_total_difficulty_is_the_running_sum_from_the_base() {
        let mut hs = vec![
            header(10, 5, None),
            header(11, 7, None),
            header(12, 0, None),
        ];
        assert!(fill_total_difficulty(&mut hs, Some(&[100])).unwrap());
        assert_eq!(hs[0].total_difficulty, Some(vec![105]));
        assert_eq!(hs[1].total_difficulty, Some(vec![112]));
        assert_eq!(hs[2].total_difficulty, None);
    }

    #[test]
    fn without_a_base_a_missing_value_is_refused_and_a_wrong_one_always_is() {
        let mut hs = vec![header(10, 5, None)];
        assert!(fill_total_difficulty(&mut hs, None).is_err());
        let mut hs = vec![header(10, 5, Some(105)), header(11, 7, Some(999))];
        assert!(fill_total_difficulty(&mut hs, Some(&[100])).is_err());
        let mut hs = vec![header(10, 5, Some(105)), header(11, 7, Some(112))];
        assert!(!fill_total_difficulty(&mut hs, None).unwrap());
    }
}
