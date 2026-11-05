//! Ethereum execution objects as consensus RLP, shared by the era1 and RPC sources.
//!
//! Era1 stores canonical header, body and receipt RLP; the RPC source rebuilds the same bytes from
//! JSON fields. `from`, `gas_used`, `contract_address` and `effective_gas_price` are not in those
//! bytes; they are filled here so the row schema can hold them. `solo clean` does not check sender
//! recovery, fees or contract addresses.

use alloy_rlp::Encodable;
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use legacy_format::headers::HeaderRow;
use legacy_format::logs::LogRow;
use legacy_format::receipts::ReceiptRow;
use legacy_format::transactions::TransactionRow;
use sha3::{Digest, Keccak256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("RLP: {0}")]
    Rlp(String),
    #[error("header: {0}")]
    Header(String),
    #[error("body: {0}")]
    Body(String),
    #[error("transaction {index} in block {block}: {reason}")]
    Transaction {
        block: u64,
        index: u32,
        reason: String,
    },
    #[error("receipts in block {block}: {reason}")]
    Receipts { block: u64, reason: String },
    #[error("signature: {0}")]
    Signature(String),
}

#[derive(Debug)]
pub struct DecodedBlock {
    pub header: HeaderRow,
    pub transactions: Vec<TransactionRow>,
    pub receipts: Vec<ReceiptRow>,
    pub logs: Vec<LogRow>,
}

pub fn decode_block(
    header_rlp: &[u8],
    body_rlp: &[u8],
    receipts_rlp: &[u8],
    total_difficulty_le: &[u8; 32],
    block_number: u64,
) -> Result<DecodedBlock, DecodeError> {
    let header = decode_header(header_rlp, block_number, total_difficulty_le)?;
    let (tx_items, ommers) = decode_body(body_rlp)?;
    let ommers_hash = keccak(&ommers);
    if ommers_hash != header.ommers_hash {
        return Err(DecodeError::Body(format!(
            "block {block_number} ommers hash does not match the uncle list"
        )));
    }
    decode_payload(header, &tx_items, receipts_rlp)
}

/// Decode a block's transactions and receipts against an already-authenticated header.
pub fn decode_payload(
    header: HeaderRow,
    tx_items: &[&[u8]],
    receipts_rlp: &[u8],
) -> Result<DecodedBlock, DecodeError> {
    let block_number = header.block_number;
    let mut transactions = Vec::with_capacity(tx_items.len());
    for (index, item) in tx_items.iter().enumerate() {
        let index =
            u32::try_from(index).map_err(|_| DecodeError::Body("too many transactions".into()))?;
        transactions.push(
            decode_transaction(item, block_number, index).map_err(|error| {
                DecodeError::Transaction {
                    block: block_number,
                    index,
                    reason: error.to_string(),
                }
            })?,
        );
    }
    let (receipts, logs) = decode_receipts(
        receipts_rlp,
        &transactions,
        block_number,
        header.base_fee_per_gas.as_deref(),
    )?;
    let cumulative = receipts
        .last()
        .map(|receipt| receipt.cumulative_gas_used)
        .unwrap_or(0);
    if cumulative != header.gas_used {
        return Err(DecodeError::Receipts {
            block: block_number,
            reason: format!(
                "receipt cumulative gas {cumulative} does not match header gas_used {}",
                header.gas_used
            ),
        });
    }
    Ok(DecodedBlock {
        header,
        transactions,
        receipts,
        logs,
    })
}

fn decode_header(
    bytes: &[u8],
    number: u64,
    total_difficulty_le: &[u8; 32],
) -> Result<HeaderRow, DecodeError> {
    let fields = list_items(bytes)?;
    if !matches!(fields.len(), 15 | 16 | 17 | 20 | 21) {
        return Err(DecodeError::Header(format!(
            "{} header fields are not an Ethereum layout this reader knows",
            fields.len()
        )));
    }
    let block_number = uint_u64(fields[8])?;
    if block_number != number {
        return Err(DecodeError::Header(format!(
            "header says block {block_number}, the era index says {number}"
        )));
    }
    let row = HeaderRow {
        block_number,
        // Hash the bytes era1 stored. verify_header then checks those fields re-encode to it.
        block_hash: keccak(bytes),
        parent_hash: fixed(fields[0])?,
        ommers_hash: fixed(fields[1])?,
        beneficiary: fixed(fields[2])?,
        state_root: fixed(fields[3])?,
        transactions_root: fixed(fields[4])?,
        receipts_root: fixed(fields[5])?,
        logs_bloom: fixed(fields[6])?,
        difficulty: uint_bytes(fields[7])?,
        gas_limit: uint_u64(fields[9])?,
        gas_used: uint_u64(fields[10])?,
        timestamp: uint_u64(fields[11])?,
        extra_data: string_payload(fields[12])?.to_vec(),
        mix_hash: fixed(fields[13])?,
        nonce: fixed(fields[14])?,
        base_fee_per_gas: fields.get(15).map(|item| uint_bytes(item)).transpose()?,
        withdrawals_root: fields.get(16).map(|item| fixed(item)).transpose()?,
        blob_gas_used: fields.get(17).map(|item| uint_u64(item)).transpose()?,
        excess_blob_gas: fields.get(18).map(|item| uint_u64(item)).transpose()?,
        parent_beacon_block_root: fields.get(19).map(|item| fixed(item)).transpose()?,
        requests_hash: fields.get(20).map(|item| fixed(item)).transpose()?,
        total_difficulty: Some(legacy_format::era1::total_difficulty_bytes(
            total_difficulty_le,
        )),
    };
    legacy_format::ethereum::verify_header(&row)
        .map_err(|error| DecodeError::Header(error.to_string()))?;
    Ok(row)
}

fn decode_body(bytes: &[u8]) -> Result<(Vec<&[u8]>, Vec<u8>), DecodeError> {
    let items = list_items(bytes)?;
    if items.len() != 2 {
        return Err(DecodeError::Body(format!(
            "body has {} RLP items; era1 blocks are transactions and uncles",
            items.len()
        )));
    }
    let transactions = list_items(items[0])?;
    Ok((transactions, items[1].to_vec()))
}

pub fn decode_transaction(
    item: &[u8],
    block_number: u64,
    index: u32,
) -> Result<TransactionRow, DecodeError> {
    let raw = transaction_envelope(item)?;
    if raw.is_empty() {
        return Err(DecodeError::Rlp("empty transaction".into()));
    }
    // EIP-2718: a legacy transaction is an RLP list (first byte >= 0xc0); anything lower is a
    // type byte. Asking `is_list` of a typed envelope misreads the type byte as a 1-byte string.
    let (tx_type, fields) = if raw[0] >= 0xc0 {
        (0u8, list_items(raw)?)
    } else {
        let fields = list_items(&raw[1..])?;
        (raw[0], fields)
    };
    let row = match tx_type {
        0 => legacy_transaction(raw, &fields, block_number, index)?,
        1 => typed_transaction(raw, 1, &fields, block_number, index)?,
        2 => typed_transaction(raw, 2, &fields, block_number, index)?,
        3 => typed_transaction(raw, 3, &fields, block_number, index)?,
        4 => typed_transaction(raw, 4, &fields, block_number, index)?,
        other => {
            return Err(DecodeError::Transaction {
                block: block_number,
                index,
                reason: format!("type {other} is not a chain ID 1 transaction type"),
            })
        }
    };
    row.canonical_bytes()
        .map_err(|error| DecodeError::Transaction {
            block: block_number,
            index,
            reason: error.to_string(),
        })?;
    Ok(row)
}

fn legacy_transaction(
    raw: &[u8],
    fields: &[&[u8]],
    block_number: u64,
    index: u32,
) -> Result<TransactionRow, DecodeError> {
    if fields.len() != 9 {
        return Err(DecodeError::Rlp(format!(
            "legacy transaction has {} fields",
            fields.len()
        )));
    }
    let (chain_id, parity) = legacy_v(fields[6])?;
    let r = scalar(fields[7])?;
    let s = scalar(fields[8])?;
    let sighash = keccak(&signing_list(&fields[..6], chain_id)?);
    let recovery_id = if parity >= 27 { parity - 27 } else { parity };
    let from = recover(&sighash, recovery_id, r, s)?;
    let stored_parity = if chain_id.is_none() {
        parity
    } else {
        recovery_id
    };
    Ok(TransactionRow {
        block_number,
        transaction_index: index,
        transaction_hash: keccak(raw),
        tx_type: 0,
        nonce: uint_u64(fields[0])?,
        from,
        to: address(fields[3])?,
        value: uint_bytes(fields[4])?,
        gas_limit: uint_u64(fields[2])?,
        gas_price: Some(uint_bytes(fields[1])?),
        max_fee_per_gas: None,
        max_priority_fee_per_gas: None,
        max_fee_per_blob_gas: None,
        input: string_payload(fields[5])?.to_vec(),
        access_list: None,
        blob_versioned_hashes: None,
        authorization_list: None,
        v_or_y_parity: Some(stored_parity),
        r: Some(r),
        s: Some(s),
        chain_id,
        source_hash: None,
        mint: None,
        is_system_tx: None,
        raw_envelope: Some(raw.to_vec()),
    })
}

fn typed_transaction(
    raw: &[u8],
    tx_type: u8,
    fields: &[&[u8]],
    block_number: u64,
    index: u32,
) -> Result<TransactionRow, DecodeError> {
    // Field positions after chain_id and nonce, per EIP-2930, 1559, 4844 and 7702.
    struct Layout {
        len: usize,
        gas_price: Option<usize>,
        fees: Option<(usize, usize)>,
        gas: usize,
        blob_fee: Option<usize>,
        blob_hashes: Option<usize>,
        authorizations: Option<usize>,
        signature: usize,
    }
    let layout = match tx_type {
        1 => Layout {
            len: 11,
            gas_price: Some(2),
            fees: None,
            gas: 3,
            blob_fee: None,
            blob_hashes: None,
            authorizations: None,
            signature: 8,
        },
        2 => Layout {
            len: 12,
            gas_price: None,
            fees: Some((2, 3)),
            gas: 4,
            blob_fee: None,
            blob_hashes: None,
            authorizations: None,
            signature: 9,
        },
        3 => Layout {
            len: 14,
            gas_price: None,
            fees: Some((2, 3)),
            gas: 4,
            blob_fee: Some(9),
            blob_hashes: Some(10),
            authorizations: None,
            signature: 11,
        },
        4 => Layout {
            len: 13,
            gas_price: None,
            fees: Some((2, 3)),
            gas: 4,
            blob_fee: None,
            blob_hashes: None,
            authorizations: Some(9),
            signature: 10,
        },
        other => return Err(DecodeError::Rlp(format!("type {other} has no layout"))),
    };
    if fields.len() != layout.len {
        return Err(DecodeError::Rlp(format!(
            "type {tx_type} transaction has {} fields",
            fields.len()
        )));
    }
    let to_i = layout.gas + 1;
    let (value_i, data_i, access_i) = (to_i + 1, to_i + 2, to_i + 3);
    let sig_at = layout.signature;
    let parity = {
        let value = uint_u64(fields[sig_at])?;
        if value > 1 {
            return Err(DecodeError::Signature(
                "typed y-parity must be 0 or 1".into(),
            ));
        }
        value as u8
    };
    let to = address(fields[to_i])?;
    if tx_type >= 3 && to.is_none() {
        return Err(DecodeError::Rlp(format!(
            "type {tx_type} transaction cannot create a contract"
        )));
    }
    let r = scalar(fields[sig_at + 1])?;
    let s = scalar(fields[sig_at + 2])?;
    let mut preimage = vec![tx_type];
    preimage.extend(list_bytes(&fields[..sig_at]));
    let from = recover(&keccak(&preimage), parity, r, s)?;
    Ok(TransactionRow {
        block_number,
        transaction_index: index,
        transaction_hash: keccak(raw),
        tx_type,
        nonce: uint_u64(fields[1])?,
        from,
        to,
        value: uint_bytes(fields[value_i])?,
        gas_limit: uint_u64(fields[layout.gas])?,
        gas_price: layout
            .gas_price
            .map(|i| uint_bytes(fields[i]))
            .transpose()?,
        max_priority_fee_per_gas: layout
            .fees
            .map(|(priority, _)| uint_bytes(fields[priority]))
            .transpose()?,
        max_fee_per_gas: layout
            .fees
            .map(|(_, fee)| uint_bytes(fields[fee]))
            .transpose()?,
        max_fee_per_blob_gas: layout.blob_fee.map(|i| uint_bytes(fields[i])).transpose()?,
        input: string_payload(fields[data_i])?.to_vec(),
        access_list: Some(require_list(fields[access_i])?.to_vec()),
        blob_versioned_hashes: layout
            .blob_hashes
            .map(|i| require_list(fields[i]).map(<[u8]>::to_vec))
            .transpose()?,
        authorization_list: layout
            .authorizations
            .map(|i| require_list(fields[i]).map(<[u8]>::to_vec))
            .transpose()?,
        v_or_y_parity: Some(parity),
        r: Some(r),
        s: Some(s),
        chain_id: Some(uint_u64(fields[0])?),
        source_hash: None,
        mint: None,
        is_system_tx: None,
        raw_envelope: Some(raw.to_vec()),
    })
}

fn decode_receipts(
    bytes: &[u8],
    transactions: &[TransactionRow],
    block_number: u64,
    base_fee: Option<&[u8]>,
) -> Result<(Vec<ReceiptRow>, Vec<LogRow>), DecodeError> {
    let items = list_items(bytes)?;
    if items.len() != transactions.len() {
        return Err(DecodeError::Receipts {
            block: block_number,
            reason: format!(
                "{} receipts for {} transactions",
                items.len(),
                transactions.len()
            ),
        });
    }
    let mut receipts = Vec::with_capacity(items.len());
    let mut logs = Vec::new();
    let mut previous = 0u64;
    let mut next_log = 0u32;
    for (index, item) in items.iter().enumerate() {
        let tx = &transactions[index];
        let (tx_type, body) = receipt_body(item)?;
        if tx_type != tx.tx_type {
            return Err(DecodeError::Receipts {
                block: block_number,
                reason: format!(
                    "transaction {} type {} has receipt type {tx_type}",
                    tx.transaction_index, tx.tx_type
                ),
            });
        }
        let fields = list_items(body)?;
        if fields.len() != 4 {
            return Err(DecodeError::Receipts {
                block: block_number,
                reason: format!(
                    "receipt {} has {} fields",
                    tx.transaction_index,
                    fields.len()
                ),
            });
        }
        let (status, post_state) =
            outcome(fields[0], tx_type).map_err(|error| DecodeError::Receipts {
                block: block_number,
                reason: error.to_string(),
            })?;
        let cumulative = uint_u64(fields[1])?;
        let gas_used = cumulative
            .checked_sub(previous)
            .ok_or_else(|| DecodeError::Receipts {
                block: block_number,
                reason: format!(
                    "transaction {} cumulative gas decreased",
                    tx.transaction_index
                ),
            })?;
        previous = cumulative;
        let receipt_logs = decode_logs(
            fields[3],
            block_number,
            tx.transaction_index,
            tx.transaction_hash,
            &mut next_log,
        )?;
        receipts.push(ReceiptRow {
            block_number,
            transaction_index: tx.transaction_index,
            transaction_hash: tx.transaction_hash,
            tx_type,
            status,
            post_state,
            cumulative_gas_used: cumulative,
            logs_bloom: fixed(fields[2])?,
            gas_used: Some(gas_used),
            contract_address: if tx.to.is_none() {
                Some(create_address(&tx.from, tx.nonce))
            } else {
                None
            },
            effective_gas_price: effective_gas_price(tx, base_fee)?,
            blob_gas_used: None,
            blob_gas_price: None,
            deposit_nonce: None,
            deposit_receipt_version: None,
            l1_fee: None,
            l1_gas_used: None,
            l1_gas_price: None,
            l1_fee_scalar: None,
        });
        logs.extend(receipt_logs);
    }
    if previous != 0 && receipts.is_empty() {
        return Err(DecodeError::Receipts {
            block: block_number,
            reason: "internal gas cursor".into(),
        });
    }
    Ok((receipts, logs))
}

fn decode_logs(
    item: &[u8],
    block_number: u64,
    transaction_index: u32,
    transaction_hash: [u8; 32],
    next_log: &mut u32,
) -> Result<Vec<LogRow>, DecodeError> {
    let mut logs = Vec::new();
    for log in list_items(item)? {
        let fields = list_items(log)?;
        if fields.len() != 3 {
            return Err(DecodeError::Receipts {
                block: block_number,
                reason: "a log is address, topics and data".into(),
            });
        }
        let mut topics = Vec::new();
        for topic in list_items(fields[1])? {
            if topics.len() == 4 {
                return Err(DecodeError::Receipts {
                    block: block_number,
                    reason: "a log has more than four topics".into(),
                });
            }
            topics.push(fixed(topic)?);
        }
        let log_index = *next_log;
        *next_log = next_log
            .checked_add(1)
            .ok_or_else(|| DecodeError::Receipts {
                block: block_number,
                reason: "log index overflowed u32".into(),
            })?;
        logs.push(LogRow {
            block_number,
            transaction_index,
            log_index,
            transaction_hash,
            address: fixed(fields[0])?,
            topics,
            data: string_payload(fields[2])?.to_vec(),
        });
    }
    Ok(logs)
}

fn receipt_body(item: &[u8]) -> Result<(u8, &[u8]), DecodeError> {
    if is_list(item)? {
        return Ok((0, item));
    }
    let payload = string_payload(item)?;
    if payload.is_empty() {
        return Err(DecodeError::Rlp("empty typed receipt".into()));
    }
    Ok((payload[0], &payload[1..]))
}

fn outcome(item: &[u8], tx_type: u8) -> Result<(Option<u8>, Option<[u8; 32]>), DecodeError> {
    let payload = string_payload(item)?;
    if payload.len() == 32 {
        if tx_type != 0 {
            return Err(DecodeError::Rlp(
                "typed receipt carries a state root".into(),
            ));
        }
        let mut state = [0u8; 32];
        state.copy_from_slice(payload);
        return Ok((None, Some(state)));
    }
    match payload {
        [] => Ok((Some(0), None)),
        [1] => Ok((Some(1), None)),
        _ => Err(DecodeError::Rlp(
            "receipt outcome is neither status nor a 32-byte state root".into(),
        )),
    }
}

fn effective_gas_price(
    tx: &TransactionRow,
    base_fee: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, DecodeError> {
    match tx.tx_type {
        0 | 1 => Ok(tx.gas_price.clone()),
        2..=4 => {
            let base = base_fee.ok_or_else(|| DecodeError::Receipts {
                block: tx.block_number,
                reason: format!("type {} receipt without a header base fee", tx.tx_type),
            })?;
            let tip =
                tx.max_priority_fee_per_gas
                    .as_deref()
                    .ok_or_else(|| DecodeError::Receipts {
                        block: tx.block_number,
                        reason: "typed fee transaction has no priority fee".into(),
                    })?;
            let max = tx
                .max_fee_per_gas
                .as_deref()
                .ok_or_else(|| DecodeError::Receipts {
                    block: tx.block_number,
                    reason: "typed fee transaction has no max fee".into(),
                })?;
            let with_tip = be_add(base, tip).map_err(|_| DecodeError::Receipts {
                block: tx.block_number,
                reason: "base fee plus priority fee overflowed".into(),
            })?;
            Ok(Some(if be_cmp(&with_tip, max).is_le() {
                with_tip
            } else {
                max.to_vec()
            }))
        }
        _ => Err(DecodeError::Receipts {
            block: tx.block_number,
            reason: format!("no effective gas price for type {}", tx.tx_type),
        }),
    }
}

fn create_address(sender: &[u8; 20], nonce: u64) -> [u8; 20] {
    let mut sender_rlp = Vec::new();
    sender.as_slice().encode(&mut sender_rlp);
    let mut nonce_rlp = Vec::new();
    nonce.encode(&mut nonce_rlp);
    let hash = keccak(&list_bytes(&[sender_rlp.as_slice(), nonce_rlp.as_slice()]));
    let mut address = [0u8; 20];
    address.copy_from_slice(&hash[12..]);
    address
}

fn signing_list(fields: &[&[u8]], chain_id: Option<u64>) -> Result<Vec<u8>, DecodeError> {
    let mut owned = Vec::new();
    if let Some(chain_id) = chain_id {
        let mut chain = Vec::new();
        chain_id.encode(&mut chain);
        let mut zero = Vec::new();
        0u8.encode(&mut zero);
        owned.push(chain);
        owned.push(zero.clone());
        owned.push(zero);
    }
    let mut items = fields.to_vec();
    for extra in &owned {
        items.push(extra);
    }
    Ok(list_bytes(&items))
}

fn legacy_v(item: &[u8]) -> Result<(Option<u64>, u8), DecodeError> {
    let value = uint_bytes(item)?;
    if be_cmp(&value, &[35]).is_lt() {
        return match value.as_slice() {
            [27] => Ok((None, 27)),
            [28] => Ok((None, 28)),
            _ => Err(DecodeError::Signature(
                "legacy v is neither 27, 28 nor EIP-155".into(),
            )),
        };
    }
    let shifted =
        be_sub(&value, &[35]).map_err(|_| DecodeError::Signature("v underflow".into()))?;
    let parity = if shifted.last().copied().unwrap_or(0) & 1 == 0 {
        0
    } else {
        1
    };
    let chain = be_to_u64(&be_shr1(&shifted))?;
    Ok((Some(chain), parity))
}

fn recover(
    sighash: &[u8; 32],
    parity: u8,
    r: [u8; 32],
    s: [u8; 32],
) -> Result<[u8; 20], DecodeError> {
    let mut signature = Signature::from_scalars(r, s)
        .map_err(|_| DecodeError::Signature("r or s is not a usable secp256k1 scalar".into()))?;
    // k256's recover path rejects a high s. Pre-EIP-2 signatures are high-s, so fold s
    // and flip the id here; the row keeps the original s and v.
    let mut parity = parity;
    if let Some(low_s) = signature.normalize_s() {
        signature = low_s;
        parity ^= 1;
    }
    let id = RecoveryId::try_from(parity)
        .map_err(|_| DecodeError::Signature("recovery id is not 0 or 1".into()))?;
    let key = VerifyingKey::recover_from_prehash(sighash, &signature, id)
        .map_err(|_| DecodeError::Signature("sender recovery failed".into()))?;
    let encoded = key.to_encoded_point(false);
    let hash = keccak(&encoded.as_bytes()[1..]);
    let mut address = [0u8; 20];
    address.copy_from_slice(&hash[12..]);
    Ok(address)
}

pub(crate) fn transaction_envelope(item: &[u8]) -> Result<&[u8], DecodeError> {
    if is_list(item)? {
        Ok(item)
    } else {
        string_payload(item)
    }
}

fn list_items(bytes: &[u8]) -> Result<Vec<&[u8]>, DecodeError> {
    let mut rest = bytes;
    match alloy_rlp::Header::decode_raw(&mut rest)
        .map_err(|error| DecodeError::Rlp(error.to_string()))?
    {
        alloy_rlp::PayloadView::List(items) if rest.is_empty() => Ok(items),
        _ => Err(DecodeError::Rlp("expected one RLP list".into())),
    }
}

pub(crate) fn list_bytes(items: &[&[u8]]) -> Vec<u8> {
    let payload_length = items.iter().map(|item| item.len()).sum();
    let mut out = Vec::new();
    alloy_rlp::Header {
        list: true,
        payload_length,
    }
    .encode(&mut out);
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

fn is_list(item: &[u8]) -> Result<bool, DecodeError> {
    let mut rest = item;
    let header = alloy_rlp::Header::decode(&mut rest)
        .map_err(|error| DecodeError::Rlp(error.to_string()))?;
    if rest.len() != header.payload_length {
        return Err(DecodeError::Rlp(
            "RLP item does not end at its payload".into(),
        ));
    }
    Ok(header.list)
}

fn string_payload(item: &[u8]) -> Result<&[u8], DecodeError> {
    let mut rest = item;
    let payload = alloy_rlp::Header::decode_bytes(&mut rest, false)
        .map_err(|error| DecodeError::Rlp(error.to_string()))?;
    if !rest.is_empty() {
        return Err(DecodeError::Rlp(
            "trailing bytes after an RLP string".into(),
        ));
    }
    Ok(payload)
}

fn require_list(item: &[u8]) -> Result<&[u8], DecodeError> {
    if is_list(item)? {
        Ok(item)
    } else {
        Err(DecodeError::Rlp("expected an RLP list".into()))
    }
}

fn fixed<const N: usize>(item: &[u8]) -> Result<[u8; N], DecodeError> {
    let payload = string_payload(item)?;
    payload
        .try_into()
        .map_err(|_| DecodeError::Rlp(format!("expected {N} bytes, found {}", payload.len())))
}

fn uint_bytes(item: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let payload = string_payload(item)?;
    if payload.len() > 32 || payload.first() == Some(&0) {
        return Err(DecodeError::Rlp("non-canonical integer".into()));
    }
    Ok(payload.to_vec())
}

fn uint_u64(item: &[u8]) -> Result<u64, DecodeError> {
    be_to_u64(&uint_bytes(item)?)
}

fn address(item: &[u8]) -> Result<Option<[u8; 20]>, DecodeError> {
    let payload = string_payload(item)?;
    if payload.is_empty() {
        return Ok(None);
    }
    payload
        .try_into()
        .map(Some)
        .map_err(|_| DecodeError::Rlp("address must be 20 bytes or empty".into()))
}

fn scalar(item: &[u8]) -> Result<[u8; 32], DecodeError> {
    let payload = uint_bytes(item)?;
    if payload.is_empty() {
        return Err(DecodeError::Signature("signature scalar is zero".into()));
    }
    let mut out = [0u8; 32];
    out[32 - payload.len()..].copy_from_slice(&payload);
    Ok(out)
}

pub(crate) fn keccak(bytes: &[u8]) -> [u8; 32] {
    Keccak256::digest(bytes).into()
}

fn be_to_u64(bytes: &[u8]) -> Result<u64, DecodeError> {
    if bytes.len() > 8 {
        return Err(DecodeError::Rlp("integer does not fit in u64".into()));
    }
    let mut wide = [0u8; 8];
    wide[8 - bytes.len()..].copy_from_slice(bytes);
    Ok(u64::from_be_bytes(wide))
}

fn be_cmp(left: &[u8], right: &[u8]) -> std::cmp::Ordering {
    let left = trim(left);
    let right = trim(right);
    left.len().cmp(&right.len()).then(left.cmp(right))
}

fn trim(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|byte| *byte != 0) {
        Some(start) => &bytes[start..],
        None => &[],
    }
}

fn be_sub(left: &[u8], right: &[u8]) -> Result<Vec<u8>, ()> {
    if be_cmp(left, right).is_lt() {
        return Err(());
    }
    let width = left.len().max(right.len());
    let mut out = vec![0u8; width];
    let mut borrow = 0i16;
    for index in 0..width {
        let l = *left.get(left.len().wrapping_sub(1 + index)).unwrap_or(&0) as i16;
        let r = *right.get(right.len().wrapping_sub(1 + index)).unwrap_or(&0) as i16;
        let mut difference = l - r - borrow;
        if difference < 0 {
            difference += 256;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out[width - 1 - index] = difference as u8;
    }
    Ok(trim(&out).to_vec())
}

fn be_shr1(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; bytes.len()];
    let mut carry = 0u8;
    for (index, byte) in bytes.iter().enumerate() {
        out[index] = (byte >> 1) | carry;
        carry = if byte & 1 == 1 { 0x80 } else { 0 };
    }
    trim(&out).to_vec()
}

fn be_add(left: &[u8], right: &[u8]) -> Result<Vec<u8>, ()> {
    let width = left.len().max(right.len());
    let mut out = vec![0u8; width + 1];
    let mut carry = 0u16;
    for index in 0..width {
        let l = *left.get(left.len().wrapping_sub(1 + index)).unwrap_or(&0) as u16;
        let r = *right.get(right.len().wrapping_sub(1 + index)).unwrap_or(&0) as u16;
        let sum = l + r + carry;
        out[width - index] = (sum & 0xff) as u8;
        carry = sum >> 8;
    }
    out[0] = carry as u8;
    if trim(&out).len() > 32 {
        return Err(());
    }
    Ok(trim(&out).to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_address_matches_an_independent_keccak() {
        // keccak(rlp([0x11; 20], 0)), last 20 bytes. Hashed outside this function.
        assert_eq!(
            hex::encode(create_address(&[0x11; 20], 0)),
            "8f7a45ebde059392e46a46dcc14ab24681a961ea"
        );
    }

    #[test]
    fn type2_effective_price_is_capped() {
        let tx = TransactionRow {
            block_number: 1,
            transaction_index: 0,
            transaction_hash: [0; 32],
            tx_type: 2,
            nonce: 0,
            from: [0; 20],
            to: None,
            value: vec![],
            gas_limit: 21_000,
            gas_price: None,
            max_fee_per_gas: Some(vec![20]),
            max_priority_fee_per_gas: Some(vec![3]),
            max_fee_per_blob_gas: None,
            input: vec![],
            access_list: Some(vec![0xc0]),
            blob_versioned_hashes: None,
            authorization_list: None,
            v_or_y_parity: Some(0),
            r: Some([1; 32]),
            s: Some([1; 32]),
            chain_id: Some(1),
            source_hash: None,
            mint: None,
            is_system_tx: None,
            raw_envelope: None,
        };
        assert_eq!(
            effective_gas_price(&tx, Some(&[10])).unwrap(),
            Some(vec![13])
        );
        assert_eq!(
            effective_gas_price(&tx, Some(&[30])).unwrap(),
            Some(vec![20])
        );
        assert_eq!(
            effective_gas_price(&tx, Some(&[1, 0])).unwrap(),
            Some(vec![20])
        );
    }
}
