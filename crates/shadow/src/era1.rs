//! One era1 file in, one relic out.
//!
//! A relic is sealed only for a complete aligned epoch of 8192 blocks. A shorter file, including
//! the partial era that stops at the merge, is refused. The accumulator root copied into the
//! manifest is the one recomputed from the headers, and it must already match the file.

use std::io::Read;
#[cfg(test)]
use std::io::Write;
use std::path::{Path, PathBuf};

use legacy_format::manifest::{Boundary, Manifest};
use legacy_format::pact;
use legacy_format::{era1 as ssz, relic, Hash32};
use snap::read::FrameDecoder;
#[cfg(test)]
use snap::write::FrameEncoder;
use thiserror::Error;

use crate::decode::{self, DecodedBlock};

const ENTRY_HEADER: usize = 8;
const VALUE_LIMIT: usize = 50 * 1024 * 1024;
// A 30M-gas pre-merge block cannot carry much more than a couple of megabytes of calldata.
// The cap rejects a hostile length, it is not an era1 rule.
const DECOMPRESSED_LIMIT: usize = 16 * 1024 * 1024;

const VERSION: u16 = 0x3265;
const COMPRESSED_HEADER: u16 = 0x03;
const COMPRESSED_BODY: u16 = 0x04;
const COMPRESSED_RECEIPTS: u16 = 0x05;
const TOTAL_DIFFICULTY: u16 = 0x06;
const ACCUMULATOR: u16 = 0x07;
const BLOCK_INDEX: u16 = 0x3266;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Decode(#[from] decode::DecodeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Format(#[from] legacy_format::Error),
    #[error(transparent)]
    Parquet(#[from] legacy_parquet::Error),
    #[error(transparent)]
    Era1(#[from] ssz::Era1Error),
    #[error(transparent)]
    Header(#[from] legacy_format::ethereum::Error),
    #[error(transparent)]
    Links(#[from] legacy_format::headers::HeaderError),
    #[error(transparent)]
    Transactions(#[from] legacy_format::ethereum_transactions::TransactionEncodingError),
    #[error(transparent)]
    Receipts(#[from] legacy_format::ethereum_receipts::ReceiptEncodingError),
    #[error(transparent)]
    Consistency(#[from] legacy_format::consistency::ConsistencyError),
}

pub struct SealRequest {
    pub file: PathBuf,
    pub out: PathBuf,
    pub from: u64,
    pub to: u64,
    pub chain_id: u64,
    pub silo: String,
    pub predecessor: Option<PathBuf>,
    /// Seal with `detached_pact_start` instead of a predecessor (RFC-0001 §8.4.1).
    pub detached: bool,
}

#[derive(Debug)]
pub struct Sealed {
    pub relic_index: u64,
    pub start: u64,
    pub end: u64,
    pub chain_id: u64,
    pub accumulator: Hash32,
    pub pact_root: Hash32,
    pub directory: PathBuf,
}

struct Entry<'a> {
    offset: usize,
    kind: u16,
    value: &'a [u8],
}

#[derive(Debug)]
struct Parsed {
    start: u64,
    blocks: Vec<DecodedBlock>,
    accumulator: [u8; 32],
}

pub fn seal(request: &SealRequest) -> Result<Sealed, Error> {
    let bytes = std::fs::read(&request.file)?;
    let parsed = parse(&bytes)?;
    let count = parsed.blocks.len() as u64;
    let end = parsed
        .start
        .checked_add(count.saturating_sub(1))
        .filter(|_| count > 0)
        .ok_or_else(|| Error::Message("era1 file has no blocks".into()))?;
    if count != relic::BLOCKS_PER_RELIC || !parsed.start.is_multiple_of(relic::BLOCKS_PER_RELIC) {
        return Err(Error::Message(format!(
            "era1 file covers blocks {}..={end} ({count} blocks); a relic is sealed only for one aligned {}-block epoch",
            parsed.start,
            relic::BLOCKS_PER_RELIC
        )));
    }
    if request.from != parsed.start || request.to != end {
        return Err(Error::Message(format!(
            "era1 file covers blocks {}..={end}, not {}..={}",
            parsed.start, request.from, request.to
        )));
    }

    let index = relic::relic_index(parsed.start);
    let predecessor = match &request.predecessor {
        Some(path) => Some(load_manifest(path)?),
        None if index == 0 => None,
        None if request.detached => None,
        None => {
            return Err(Error::Message(format!(
                "relic {index} is not the genesis relic; pass --after with the predecessor manifest, or --detached"
            )))
        }
    };

    let headers: Vec<_> = parsed
        .blocks
        .iter()
        .map(|block| block.header.clone())
        .collect();
    let transactions: Vec<_> = parsed
        .blocks
        .iter()
        .flat_map(|block| block.transactions.iter().cloned())
        .collect();
    let receipts: Vec<_> = parsed
        .blocks
        .iter()
        .flat_map(|block| block.receipts.iter().cloned())
        .collect();
    let logs: Vec<_> = parsed
        .blocks
        .iter()
        .flat_map(|block| block.logs.iter().cloned())
        .collect();
    let boundary = Boundary {
        start_block_hash: Hash32::new(headers[0].block_hash),
        end_block_hash: Hash32::new(headers[headers.len() - 1].block_hash),
        parent_hash_of_start: Hash32::new(headers[0].parent_hash),
    };
    let range = relic::relic_range(index);
    legacy_format::headers::verify_relic_rows(&headers, range, boundary)?;
    for header in &headers {
        legacy_format::ethereum::verify_header(header)?;
    }
    legacy_format::ethereum_transactions::verify_transaction_roots(&headers, &transactions)?;
    legacy_format::ethereum_receipts::verify_receipt_roots(&headers, &receipts, &logs)?;
    legacy_format::bloom::receipt_blooms(&logs, &receipts)?;
    legacy_format::bloom::header_blooms(&headers, &receipts)?;
    legacy_format::consistency::ethereum_receipt_gas(&headers, &receipts)?;
    ssz::verify(&headers, Hash32::new(parsed.accumulator))?;

    prepare_output(&request.out)?;
    let (header_bytes, header_entry) = legacy_parquet::headers::write_headers(&headers)?;
    let (tx_bytes, tx_entry) = legacy_parquet::transactions::write_transactions(&transactions)?;
    let (receipt_bytes, receipt_entry) = legacy_parquet::receipts::write_receipts(&receipts)?;
    let (log_bytes, log_entry) = legacy_parquet::logs::write_logs(&logs)?;
    for (name, bytes) in [
        (header_entry.name.as_str(), header_bytes.as_slice()),
        (tx_entry.name.as_str(), tx_bytes.as_slice()),
        (receipt_entry.name.as_str(), receipt_bytes.as_slice()),
        (log_entry.name.as_str(), log_bytes.as_slice()),
    ] {
        std::fs::write(request.out.join(name), bytes)?;
    }

    let mut manifest = Manifest::new(request.chain_id, &request.silo, index, boundary);
    manifest.detached_pact_start = request.detached && predecessor.is_none() && index != 0;
    manifest.era1_accumulator_root = Some(Hash32::new(parsed.accumulator));
    manifest.files = vec![header_entry, tx_entry, receipt_entry, log_entry];
    let pact_root = pact::seal_chain(predecessor.as_ref(), std::slice::from_mut(&mut manifest))?;
    std::fs::write(
        request.out.join("manifest.json"),
        manifest.to_canonical_bytes()?,
    )?;

    Ok(Sealed {
        relic_index: index,
        start: parsed.start,
        end,
        chain_id: request.chain_id,
        accumulator: Hash32::new(parsed.accumulator),
        pact_root,
        directory: request.out.clone(),
    })
}

fn parse(file: &[u8]) -> Result<Parsed, Error> {
    let entries = scan(file)?;
    if entries.is_empty() {
        return Err(Error::Message("era1 file is empty".into()));
    }
    if entries[0].kind != VERSION || !entries[0].value.is_empty() {
        return Err(Error::Message(
            "era1 file does not start with an empty e2store version entry".into(),
        ));
    }

    let mut cursor = 1;
    let mut tuples = Vec::new();
    while cursor + 3 < entries.len() && entries[cursor].kind == COMPRESSED_HEADER {
        let kinds = [
            entries[cursor].kind,
            entries[cursor + 1].kind,
            entries[cursor + 2].kind,
            entries[cursor + 3].kind,
        ];
        if kinds
            != [
                COMPRESSED_HEADER,
                COMPRESSED_BODY,
                COMPRESSED_RECEIPTS,
                TOTAL_DIFFICULTY,
            ]
        {
            return Err(Error::Message(
                "era1 block tuple is not header, body, receipts, total difficulty".into(),
            ));
        }
        if entries[cursor + 3].value.len() != 32 {
            return Err(Error::Message(
                "era1 total difficulty is not 32 bytes".into(),
            ));
        }
        tuples.push(cursor);
        cursor += 4;
    }
    while cursor < entries.len()
        && entries[cursor].kind != ACCUMULATOR
        && entries[cursor].kind != BLOCK_INDEX
    {
        if is_block_type(entries[cursor].kind) {
            return Err(Error::Message(
                "era1 block entry appears after the block tuples".into(),
            ));
        }
        cursor += 1;
    }
    if cursor + 1 >= entries.len()
        || entries[cursor].kind != ACCUMULATOR
        || entries[cursor + 1].kind != BLOCK_INDEX
        || cursor + 2 != entries.len()
    {
        return Err(Error::Message(
            "era1 file does not end in an accumulator and a block index".into(),
        ));
    }
    let accumulator = entries[cursor].value;
    if accumulator.len() != 32 {
        return Err(Error::Message("era1 accumulator is not 32 bytes".into()));
    }
    let mut accumulator_root = [0u8; 32];
    accumulator_root.copy_from_slice(accumulator);

    let index_entry = &entries[cursor + 1];
    let index = read_block_index(index_entry.value)?;
    if index.count != tuples.len() as u64 {
        return Err(Error::Message(format!(
            "block index counts {} blocks but the file has {} tuples",
            index.count,
            tuples.len()
        )));
    }
    for (nth, tuple) in tuples.iter().enumerate() {
        let relative = index.offsets[nth];
        let absolute = (index_entry.offset as i64)
            .checked_add(relative)
            .ok_or_else(|| Error::Message(format!("block index offset {nth} overflows")))?;
        if absolute < 0 || absolute as usize != entries[*tuple].offset {
            return Err(Error::Message(format!(
                "block index offset {nth} does not point at its compressed header"
            )));
        }
    }

    let mut blocks = Vec::with_capacity(tuples.len());
    for (nth, tuple) in tuples.iter().enumerate() {
        let number = index
            .start
            .checked_add(nth as u64)
            .ok_or_else(|| Error::Message("era1 block number overflowed".into()))?;
        let mut difficulty = [0u8; 32];
        difficulty.copy_from_slice(entries[tuple + 3].value);
        blocks.push(decode::decode_block(
            &decompress(entries[*tuple].value)?,
            &decompress(entries[tuple + 1].value)?,
            &decompress(entries[tuple + 2].value)?,
            &difficulty,
            number,
        )?);
    }
    for pair in blocks.windows(2) {
        if pair[1].header.parent_hash != pair[0].header.block_hash {
            return Err(Error::Message(format!(
                "stored parent link breaks at block {}",
                pair[1].header.block_number
            )));
        }
    }

    let headers: Vec<_> = blocks.iter().map(|block| block.header.clone()).collect();
    let computed = ssz::accumulator_from_headers(&headers)?;
    if computed != accumulator_root {
        return Err(Error::Message(format!(
            "era1 accumulator {} does not match headers {}",
            hex::encode(accumulator_root),
            hex::encode(computed)
        )));
    }
    Ok(Parsed {
        start: index.start,
        blocks,
        accumulator: accumulator_root,
    })
}

fn is_block_type(kind: u16) -> bool {
    matches!(
        kind,
        COMPRESSED_HEADER | COMPRESSED_BODY | COMPRESSED_RECEIPTS | TOTAL_DIFFICULTY
    )
}

struct BlockIndex {
    start: u64,
    count: u64,
    offsets: Vec<i64>,
}

fn read_block_index(value: &[u8]) -> Result<BlockIndex, Error> {
    if value.len() < 16 || !(value.len() - 16).is_multiple_of(8) {
        return Err(Error::Message("era1 block index has a bad length".into()));
    }
    let start = u64::from_le_bytes(value[..8].try_into().expect("8 bytes"));
    let count = u64::from_le_bytes(value[value.len() - 8..].try_into().expect("8 bytes"));
    let slots = (value.len() - 16) / 8;
    if count as usize != slots {
        return Err(Error::Message(format!(
            "block index count {count} does not match its {slots} offsets"
        )));
    }
    let mut offsets = Vec::with_capacity(slots);
    for slot in 0..slots {
        let start = 8 + slot * 8;
        // Relative to the first byte of the index record, not to the offset's own position.
        // Geth stores `header_offset - index_offset`, which is negative.
        offsets.push(i64::from_le_bytes(
            value[start..start + 8].try_into().expect("8 bytes"),
        ));
    }
    Ok(BlockIndex {
        start,
        count,
        offsets,
    })
}

fn scan(file: &[u8]) -> Result<Vec<Entry<'_>>, Error> {
    let mut entries = Vec::new();
    let mut offset = 0;
    while offset < file.len() {
        if file.len() - offset < ENTRY_HEADER {
            return Err(Error::Message("truncated e2store header".into()));
        }
        let kind = u16::from_le_bytes([file[offset], file[offset + 1]]);
        let len =
            u32::from_le_bytes(file[offset + 2..offset + 6].try_into().expect("4 bytes")) as usize;
        if file[offset + 6] != 0 || file[offset + 7] != 0 {
            return Err(Error::Message("e2store reserved bytes are not zero".into()));
        }
        if len > VALUE_LIMIT {
            return Err(Error::Message(format!(
                "e2store entry is {len} bytes, over the {VALUE_LIMIT}-byte limit"
            )));
        }
        let value_at = offset + ENTRY_HEADER;
        let end = value_at
            .checked_add(len)
            .ok_or_else(|| Error::Message("e2store entry length overflowed".into()))?;
        if end > file.len() {
            return Err(Error::Message("truncated e2store value".into()));
        }
        entries.push(Entry {
            offset,
            kind,
            value: &file[value_at..end],
        });
        offset = end;
    }
    Ok(entries)
}

fn decompress(framed: &[u8]) -> Result<Vec<u8>, Error> {
    let mut decoder = FrameDecoder::new(framed);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = decoder.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if out.len().saturating_add(n) > DECOMPRESSED_LIMIT {
            return Err(Error::Message(format!(
                "decompressed era1 entry exceeds {DECOMPRESSED_LIMIT} bytes"
            )));
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

pub(crate) fn load_manifest(path: &Path) -> Result<Manifest, Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| Error::Message(format!("reading {}: {error}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|error| Error::Message(format!("parsing {}: {error}", path.display())))
}

pub(crate) fn prepare_output(dir: &Path) -> Result<(), Error> {
    if dir.exists() {
        if !dir.is_dir() {
            return Err(Error::Message(format!(
                "{} exists and is not a directory",
                dir.display()
            )));
        }
        if std::fs::read_dir(dir)?.next().is_some() {
            return Err(Error::Message(format!(
                "{} is not empty; a sealed relic is not overwritten",
                dir.display()
            )));
        }
    } else {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

#[cfg(test)]
fn write_entry(out: &mut Vec<u8>, kind: u16, value: &[u8]) {
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(value);
}

#[cfg(test)]
fn write_framed(out: &mut Vec<u8>, kind: u16, raw: &[u8]) -> Result<(), Error> {
    let mut encoder = FrameEncoder::new(Vec::new());
    encoder.write_all(raw)?;
    let framed = encoder
        .into_inner()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    write_entry(out, kind, &framed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::list_bytes;
    use alloy_rlp::Encodable;
    use legacy_format::ethereum::{header_hash, header_rlp};
    use legacy_format::headers::HeaderRow;
    use sha3::Digest;

    fn difficulty_le(block: u64) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[..8].copy_from_slice(&(block + 1).to_le_bytes());
        out
    }

    fn empty_header(number: u64, parent: [u8; 32], base_fee: Option<Vec<u8>>) -> HeaderRow {
        let empty_root: [u8; 32] =
            alloy_trie::root::ordered_trie_root_encoded::<Vec<u8>>(&[]).into();
        let mut row = HeaderRow {
            block_number: number,
            block_hash: [0; 32],
            parent_hash: parent,
            ommers_hash: keccak(&[0xc0]),
            beneficiary: [0; 20],
            state_root: [0; 32],
            transactions_root: empty_root,
            receipts_root: empty_root,
            logs_bloom: [0; 256],
            difficulty: vec![1],
            gas_limit: 30_000_000,
            gas_used: 0,
            timestamp: number,
            extra_data: vec![],
            mix_hash: [0; 32],
            nonce: [0; 8],
            base_fee_per_gas: base_fee,
            withdrawals_root: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
            requests_hash: None,
            total_difficulty: Some(ssz::total_difficulty_bytes(&difficulty_le(number))),
        };
        row.block_hash = header_hash(&row).unwrap();
        row
    }

    fn keccak(bytes: &[u8]) -> [u8; 32] {
        sha3::Keccak256::digest(bytes).into()
    }

    fn rlp_u64(value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        value.encode(&mut out);
        out
    }

    fn trim_zeros(bytes: &[u8]) -> &[u8] {
        match bytes.iter().position(|byte| *byte != 0) {
            Some(start) => &bytes[start..],
            None => &[],
        }
    }

    fn rlp_bytes(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        bytes.encode(&mut out);
        out
    }

    fn append_block(
        file: &mut Vec<u8>,
        header_at: &mut Vec<usize>,
        header: &HeaderRow,
        body: &[u8],
        receipts: &[u8],
    ) {
        header_at.push(file.len());
        write_framed(file, COMPRESSED_HEADER, &header_rlp(header).unwrap()).unwrap();
        write_framed(file, COMPRESSED_BODY, body).unwrap();
        write_framed(file, COMPRESSED_RECEIPTS, receipts).unwrap();
        write_entry(file, TOTAL_DIFFICULTY, &difficulty_le(header.block_number));
    }

    fn finish(file: &mut Vec<u8>, start: u64, header_at: &[usize], headers: &[HeaderRow]) {
        let root = ssz::accumulator_from_headers(headers).unwrap();
        write_entry(file, ACCUMULATOR, &root);
        let index_at = file.len();
        let mut index = Vec::new();
        index.extend_from_slice(&start.to_le_bytes());
        for offset in header_at {
            let relative = *offset as i64 - index_at as i64;
            index.extend_from_slice(&relative.to_le_bytes());
        }
        index.extend_from_slice(&(header_at.len() as u64).to_le_bytes());
        write_entry(file, BLOCK_INDEX, &index);
    }

    fn empty_body() -> Vec<u8> {
        list_bytes(&[&[0xc0][..], &[0xc0][..]])
    }

    #[test]
    fn a_short_era_decodes_and_is_not_a_relic() {
        let header = empty_header(0, [0xab; 32], None);
        let mut file = Vec::new();
        write_entry(&mut file, VERSION, &[]);
        let mut offsets = Vec::new();
        append_block(&mut file, &mut offsets, &header, &empty_body(), &[0xc0]);
        finish(&mut file, 0, &offsets, std::slice::from_ref(&header));

        let parsed = parse(&file).unwrap();
        assert_eq!(parsed.blocks.len(), 1);
        assert_eq!(parsed.blocks[0].header, header);
        assert!(parsed.blocks[0].transactions.is_empty());

        let dir = std::env::temp_dir().join(format!("era1-short-{}", std::process::id()));
        let error = seal(&SealRequest {
            file: {
                let path = dir.join("short.era1");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(&path, &file).unwrap();
                path
            },
            out: dir.join("out"),
            from: 0,
            to: 0,
            chain_id: 1,
            silo: "ethereum".into(),
            predecessor: None,
            detached: false,
        })
        .unwrap_err();
        let Error::Message(text) = error else {
            panic!("expected a refusal, got {error}");
        };
        assert!(text.contains("aligned"), "{text}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn reserved_bytes_and_a_bad_accumulator_are_refused() {
        let mut file = Vec::new();
        write_entry(&mut file, VERSION, &[]);
        file[6] = 1;
        let error = parse(&file).unwrap_err();
        assert!(error.to_string().contains("reserved"), "{error}");

        let header = empty_header(4, [0; 32], Some(vec![0x0a]));
        let mut file = Vec::new();
        write_entry(&mut file, VERSION, &[]);
        let mut offsets = Vec::new();
        append_block(&mut file, &mut offsets, &header, &empty_body(), &[0xc0]);
        finish(&mut file, 4, &offsets, &[header]);
        // One block: index value is start, one offset and count (24 bytes) plus its 8-byte header.
        // The accumulator value sits immediately before that.
        let accumulator_value = file.len() - (ENTRY_HEADER + 24) - 32;
        file[accumulator_value] ^= 0xff;
        let error = parse(&file).unwrap_err();
        assert!(error.to_string().contains("accumulator"), "{error}");
    }

    #[test]
    fn an_aligned_epoch_seals() {
        let mut headers = Vec::with_capacity(8192);
        let mut parent = [0x10; 32];
        for number in 0..8192 {
            let header = empty_header(number, parent, None);
            parent = header.block_hash;
            headers.push(header);
        }
        let mut file = Vec::new();
        write_entry(&mut file, VERSION, &[]);
        let mut offsets = Vec::new();
        for header in &headers {
            append_block(&mut file, &mut offsets, header, &empty_body(), &[0xc0]);
        }
        finish(&mut file, 0, &offsets, &headers);

        let dir = std::env::temp_dir().join(format!("era1-epoch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let era_path = dir.join("00000.era1");
        std::fs::write(&era_path, &file).unwrap();
        let out = dir.join("relic");
        let sealed = seal(&SealRequest {
            file: era_path,
            out: out.clone(),
            from: 0,
            to: 8191,
            chain_id: 1,
            silo: "ethereum".into(),
            predecessor: None,
            detached: false,
        })
        .unwrap();
        assert_eq!(sealed.relic_index, 0);
        assert_eq!(
            sealed.accumulator.as_bytes(),
            &ssz::accumulator_from_headers(&headers).unwrap()
        );

        let manifest: Manifest =
            serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest.era1_accumulator_root, Some(sealed.accumulator));
        assert_eq!(manifest.files.len(), 4);
        assert!(manifest
            .files
            .iter()
            .all(|file| file.table != legacy_format::Table::Withdrawals));

        let read = legacy_parquet::headers::read_headers(bytes::Bytes::from(
            std::fs::read(out.join("headers.parquet")).unwrap(),
        ))
        .unwrap();
        assert_eq!(read, headers);
        ssz::verify(&read, sealed.accumulator).unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn legacy_transaction_receipt_and_create_address_round_trip() {
        use k256::ecdsa::SigningKey;

        let secret = [0x42u8; 32];
        let key = SigningKey::from_bytes((&secret).into()).unwrap();
        let point = key.verifying_key().to_encoded_point(false);
        let sender_hash = keccak(&point.as_bytes()[1..]);
        let mut sender = [0u8; 20];
        sender.copy_from_slice(&sender_hash[12..]);

        let nonce = rlp_u64(0);
        let gas_price = rlp_u64(1);
        let gas = rlp_u64(21_000);
        let to = rlp_bytes(&[0x22; 20]);
        let value = rlp_u64(0);
        let data = rlp_bytes(&[]);
        let sighash = keccak(&list_bytes(&[
            nonce.as_slice(),
            gas_price.as_slice(),
            gas.as_slice(),
            to.as_slice(),
            value.as_slice(),
            data.as_slice(),
        ]));
        let (signature, id) = key.sign_prehash_recoverable(&sighash).unwrap();
        let compact = signature.to_bytes();
        let r = rlp_bytes(trim_zeros(&compact[..32]));
        let s = rlp_bytes(trim_zeros(&compact[32..]));
        let v = rlp_u64(u64::from(27 + id.to_byte()));
        let raw = list_bytes(&[
            nonce.as_slice(),
            gas_price.as_slice(),
            gas.as_slice(),
            to.as_slice(),
            value.as_slice(),
            data.as_slice(),
            v.as_slice(),
            r.as_slice(),
            s.as_slice(),
        ]);
        let tx_root: [u8; 32] =
            alloy_trie::root::ordered_trie_root_encoded(std::slice::from_ref(&raw)).into();

        let log_address = rlp_bytes(&[0x33; 20]);
        let topic = rlp_bytes(&[0x44; 32]);
        let topics = list_bytes(&[topic.as_slice()]);
        let log_data = rlp_bytes(&[0x01, 0x02]);
        let log = list_bytes(&[
            log_address.as_slice(),
            topics.as_slice(),
            log_data.as_slice(),
        ]);
        let logs = list_bytes(&[log.as_slice()]);
        let bloom = legacy_format::bloom::logs_bloom(&[legacy_format::logs::LogRow {
            block_number: 1,
            transaction_index: 0,
            log_index: 0,
            transaction_hash: [0; 32],
            address: [0x33; 20],
            topics: vec![[0x44; 32]],
            data: vec![0x01, 0x02],
        }])
        .unwrap();
        let status = rlp_u64(1);
        let cumulative = rlp_u64(21_000);
        let bloom_rlp = rlp_bytes(&bloom);
        let receipt = list_bytes(&[
            status.as_slice(),
            cumulative.as_slice(),
            bloom_rlp.as_slice(),
            logs.as_slice(),
        ]);
        let receipts = list_bytes(&[receipt.as_slice()]);
        let receipt_root: [u8; 32] =
            alloy_trie::root::ordered_trie_root_encoded(std::slice::from_ref(&receipt)).into();
        let mut header = empty_header(1, [0x55; 32], None);
        header.transactions_root = tx_root;
        header.receipts_root = receipt_root;
        header.logs_bloom = bloom;
        header.gas_used = 21_000;
        header.block_hash = header_hash(&header).unwrap();
        let body = list_bytes(&[list_bytes(&[raw.as_slice()]).as_slice(), &[0xc0][..]]);

        let decoded = decode::decode_block(
            &header_rlp(&header).unwrap(),
            &body,
            &receipts,
            &difficulty_le(1),
            1,
        )
        .unwrap();
        assert_eq!(decoded.transactions[0].from, sender);
        assert_eq!(decoded.transactions[0].to, Some([0x22; 20]));
        assert_eq!(
            decoded.transactions[0].raw_envelope.as_deref(),
            Some(raw.as_slice())
        );
        assert_eq!(decoded.receipts[0].gas_used, Some(21_000));
        assert_eq!(decoded.receipts[0].effective_gas_price, Some(vec![1]));
        assert_eq!(decoded.receipts[0].status, Some(1));
        assert_eq!(
            decoded.logs,
            vec![legacy_format::logs::LogRow {
                block_number: 1,
                transaction_index: 0,
                log_index: 0,
                transaction_hash: decoded.transactions[0].transaction_hash,
                address: [0x33; 20],
                topics: vec![[0x44; 32]],
                data: vec![0x01, 0x02],
            }]
        );
        assert!(decoded.receipts[0].contract_address.is_none());
        legacy_format::bloom::receipt_blooms(&decoded.logs, &decoded.receipts).unwrap();
        legacy_format::ethereum_receipts::verify_receipt_roots(
            std::slice::from_ref(&decoded.header),
            &decoded.receipts,
            &decoded.logs,
        )
        .unwrap();
    }

    #[test]
    fn high_s_legacy_signature_recovers_the_signer() {
        use k256::ecdsa::SigningKey;

        let secret = [0x42u8; 32];
        let key = SigningKey::from_bytes((&secret).into()).unwrap();
        let point = key.verifying_key().to_encoded_point(false);
        let sender_hash = keccak(&point.as_bytes()[1..]);
        let mut sender = [0u8; 20];
        sender.copy_from_slice(&sender_hash[12..]);

        let nonce = rlp_u64(0);
        let gas_price = rlp_u64(1);
        let gas = rlp_u64(21_000);
        let to = rlp_bytes(&[0x22; 20]);
        let value = rlp_u64(0);
        let data = rlp_bytes(&[]);
        let sighash = keccak(&list_bytes(&[
            nonce.as_slice(),
            gas_price.as_slice(),
            gas.as_slice(),
            to.as_slice(),
            value.as_slice(),
            data.as_slice(),
        ]));
        let (signature, id) = key.sign_prehash_recoverable(&sighash).unwrap();
        assert!(signature.normalize_s().is_none());
        let compact = signature.to_bytes();
        let mut r_raw = [0u8; 32];
        r_raw.copy_from_slice(&compact[..32]);
        let high = -signature.s();
        let mut s_raw = [0u8; 32];
        s_raw.copy_from_slice(high.as_ref().to_bytes().as_slice());
        let r = rlp_bytes(trim_zeros(&r_raw));
        let s = rlp_bytes(trim_zeros(&s_raw));
        let flipped = id.to_byte() ^ 1;
        let v = rlp_u64(u64::from(27 + flipped));
        let raw = list_bytes(&[
            nonce.as_slice(),
            gas_price.as_slice(),
            gas.as_slice(),
            to.as_slice(),
            value.as_slice(),
            data.as_slice(),
            v.as_slice(),
            r.as_slice(),
            s.as_slice(),
        ]);
        let logs = list_bytes(&[]);
        let receipt = list_bytes(&[
            rlp_u64(1).as_slice(),
            rlp_u64(21_000).as_slice(),
            rlp_bytes(&[0u8; 256]).as_slice(),
            logs.as_slice(),
        ]);
        let receipts = list_bytes(&[receipt.as_slice()]);
        let mut header = empty_header(1, [0x55; 32], None);
        header.gas_used = 21_000;
        header.block_hash = header_hash(&header).unwrap();
        let body = list_bytes(&[list_bytes(&[raw.as_slice()]).as_slice(), &[0xc0][..]]);

        let decoded = decode::decode_block(
            &header_rlp(&header).unwrap(),
            &body,
            &receipts,
            &difficulty_le(1),
            1,
        )
        .unwrap();
        assert_eq!(decoded.transactions[0].from, sender);
        assert_eq!(decoded.transactions[0].s, Some(s_raw));
        assert_eq!(decoded.transactions[0].v_or_y_parity, Some(27u8 + flipped));
    }
}
