//! A synthetic, self-consistent mainnet-shaped relic and a local JSON-RPC server that serves it,
//! for the RFC-0004 M1 gate. Nothing here touches the network.
//!
//! Envelopes, receipts and headers are encoded here independently of `block.rs`, signed with a
//! fixed key, and committed with real tries, so the Shadow's rebuild has to agree with a separate
//! encoder rather than with itself.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, OnceLock};

use alloy_rlp::Encodable;
use k256::ecdsa::SigningKey;
use legacy_format::headers::HeaderRow;
use legacy_format::logs::LogRow;
use legacy_format::relic::{self, BlockRange};
use legacy_format::withdrawals::WithdrawalRow;
use serde_json::{json, Value};

use crate::decode::{keccak, list_bytes};

pub const RELIC: u64 = 2079;
pub const SHAPELLA: u64 = 17_031_168 + 3;
pub const CANCUN: u64 = 17_031_168 + 6;
pub const PRAGUE: u64 = 17_031_168 + 9;

fn rlp<T: Encodable + ?Sized>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    v.encode(&mut out);
    out
}

fn h(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

fn q(v: u64) -> String {
    format!("0x{v:x}")
}

fn qb(bytes: &[u8]) -> String {
    let trimmed: Vec<u8> = bytes.iter().copied().skip_while(|b| *b == 0).collect();
    if trimmed.is_empty() {
        "0x0".into()
    } else {
        let s = hex::encode(&trimmed);
        format!("0x{}", s.trim_start_matches('0'))
    }
}

fn root(values: &[Vec<u8>]) -> [u8; 32] {
    alloy_trie::root::ordered_trie_root_encoded(values).into()
}

struct Tx {
    kind: u8,
    protected: bool,
    to: Option<[u8; 20]>,
    nonce: u64,
    logs: usize,
}

const CHAIN: u64 = 1;
const BASE_FEE: u64 = 7_000_000_000;
const GAS_PRICE: u64 = 9_000_000_000;
const TIP: u64 = 1_000_000_000;
const MAX_FEE: u64 = 20_000_000_000;

/// The block a test can corrupt or watch; it carries one of each transaction type at Prague.
pub const BUSY: u64 = PRAGUE + 1;

fn plan(number: u64) -> Vec<Tx> {
    let to = Some([0x11; 20]);
    let mut txs = Vec::new();
    let mut nonce = number * 16;
    let mut push = |kind, protected, to, logs| {
        txs.push(Tx {
            kind,
            protected,
            to,
            nonce,
            logs,
        });
        nonce += 1;
    };
    if number == 17_031_168 + 1 {
        push(0, false, to, 1);
        push(0, true, None, 0);
    }
    if number == SHAPELLA + 1 {
        push(1, true, to, 2);
        push(2, true, to, 0);
    }
    if number == BUSY {
        push(0, true, to, 0);
        push(1, true, to, 1);
        push(2, true, None, 0);
        push(3, true, to, 3);
        push(4, true, to, 1);
    }
    if number % 1024 == 5 {
        push(2, true, to, 1);
    }
    txs
}

fn key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32].into()).expect("fixed key")
}

fn sender() -> [u8; 20] {
    let point = key().verifying_key().to_encoded_point(false);
    let hash = keccak(&point.as_bytes()[1..]);
    hash[12..].try_into().expect("20 bytes")
}

fn sign(digest: &[u8; 32]) -> (u8, Vec<u8>, Vec<u8>) {
    let (sig, id) = key().sign_prehash_recoverable(digest).expect("signs");
    let bytes = sig.to_bytes();
    let trim = |b: &[u8]| {
        b.iter()
            .copied()
            .skip_while(|x| *x == 0)
            .collect::<Vec<u8>>()
    };
    (id.to_byte(), trim(&bytes[..32]), trim(&bytes[32..]))
}

fn access_list() -> (Vec<u8>, Value) {
    let address = [0x22u8; 20];
    let slot = [0x33u8; 32];
    let item = list_bytes(&[&rlp(&address[..]), &list_bytes(&[&rlp(&slot[..])])]);
    (
        list_bytes(&[&item]),
        json!([{"address": h(&address), "storageKeys": [h(&slot)]}]),
    )
}

/// Returns (envelope, served JSON without block context).
fn transaction(number: u64, tx: &Tx) -> (Vec<u8>, Value) {
    let to_rlp = rlp(tx.to.as_ref().map(|a| &a[..]).unwrap_or(&[]));
    let value = 1_000u64 + number;
    let input = vec![0xab, 0xcd];
    let gas = 100_000u64;
    let mut json = json!({
        "nonce": q(tx.nonce),
        "from": h(&sender()),
        "to": tx.to.map(|a| h(&a)),
        "value": q(value),
        "gas": q(gas),
        "input": h(&input),
    });
    let common = |fields: &mut Vec<Vec<u8>>| {
        fields.push(rlp(&gas));
        fields.push(to_rlp.clone());
        fields.push(rlp(&value));
        fields.push(rlp(&input[..]));
    };
    let envelope = if tx.kind == 0 {
        let mut fields = vec![rlp(&tx.nonce), rlp(&GAS_PRICE)];
        common(&mut fields);
        let mut unsigned = fields.clone();
        if tx.protected {
            unsigned.extend([rlp(&CHAIN), rlp(&0u8), rlp(&0u8)]);
        }
        let refs: Vec<&[u8]> = unsigned.iter().map(Vec::as_slice).collect();
        let (parity, r, s) = sign(&keccak(&list_bytes(&refs)));
        let v = if tx.protected {
            CHAIN * 2 + 35 + u64::from(parity)
        } else {
            27 + u64::from(parity)
        };
        fields.extend([rlp(&v), rlp(&r[..]), rlp(&s[..])]);
        json["type"] = json!("0x0");
        json["gasPrice"] = json!(q(GAS_PRICE));
        json["v"] = json!(q(v));
        json["r"] = json!(qb(&r));
        json["s"] = json!(qb(&s));
        if tx.protected {
            json["chainId"] = json!(q(CHAIN));
        }
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        list_bytes(&refs)
    } else {
        let (access, access_json) = access_list();
        let mut fields = vec![rlp(&CHAIN), rlp(&tx.nonce)];
        if tx.kind == 1 {
            fields.push(rlp(&GAS_PRICE));
            json["gasPrice"] = json!(q(GAS_PRICE));
        } else {
            fields.push(rlp(&TIP));
            fields.push(rlp(&MAX_FEE));
            json["maxPriorityFeePerGas"] = json!(q(TIP));
            json["maxFeePerGas"] = json!(q(MAX_FEE));
            // Nodes also serve gasPrice (the effective price) on typed fee transactions.
            json["gasPrice"] = json!(q(BASE_FEE + TIP));
        }
        common(&mut fields);
        fields.push(access);
        json["accessList"] = access_json;
        if tx.kind == 3 {
            let hash = [0x01u8; 32];
            fields.push(rlp(&3u64));
            fields.push(list_bytes(&[&rlp(&hash[..])]));
            json["maxFeePerBlobGas"] = json!("0x3");
            json["blobVersionedHashes"] = json!([h(&hash)]);
        }
        if tx.kind == 4 {
            let (ap, ar, as_) = sign(&[0x44; 32]);
            let auth = list_bytes(&[
                &rlp(&CHAIN),
                &rlp(&[0x55u8; 20][..]),
                &rlp(&9u64),
                &rlp(&ap),
                &rlp(&ar[..]),
                &rlp(&as_[..]),
            ]);
            fields.push(list_bytes(&[&auth]));
            json["authorizationList"] = json!([{
                "chainId": q(CHAIN), "address": h(&[0x55; 20]), "nonce": "0x9",
                "yParity": q(u64::from(ap)), "r": qb(&ar), "s": qb(&as_)
            }]);
        }
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        let mut preimage = vec![tx.kind];
        preimage.extend(list_bytes(&refs));
        let (parity, r, s) = sign(&keccak(&preimage));
        let mut signed = fields.clone();
        signed.extend([rlp(&parity), rlp(&r[..]), rlp(&s[..])]);
        let refs: Vec<&[u8]> = signed.iter().map(Vec::as_slice).collect();
        let mut out = vec![tx.kind];
        out.extend(list_bytes(&refs));
        json["type"] = json!(q(u64::from(tx.kind)));
        json["chainId"] = json!(q(CHAIN));
        json["yParity"] = json!(q(u64::from(parity)));
        json["v"] = json!(q(u64::from(parity)));
        json["r"] = json!(qb(&r));
        json["s"] = json!(qb(&s));
        out
    };
    json["hash"] = json!(h(&keccak(&envelope)));
    (envelope, json)
}

fn create_address(nonce: u64) -> [u8; 20] {
    let hash = keccak(&list_bytes(&[&rlp(&sender()[..]), &rlp(&nonce)]));
    hash[12..].try_into().expect("20 bytes")
}

pub struct Fixture {
    pub blocks: Vec<(Value, Value)>,
}

pub fn chain() -> &'static Fixture {
    static CHAIN_FIXTURE: OnceLock<Fixture> = OnceLock::new();
    CHAIN_FIXTURE.get_or_init(build)
}

pub fn range() -> BlockRange {
    relic::relic_range(RELIC)
}

fn build() -> Fixture {
    let range = range();
    let mut parent = [0x99u8; 32];
    let mut withdrawal_index = 0u64;
    let mut blocks = Vec::new();
    for number in range.start..=range.end {
        let plans = plan(number);
        let mut envelopes = Vec::new();
        let mut txs_json = Vec::new();
        let mut receipt_values = Vec::new();
        let mut receipts_json = Vec::new();
        let mut all_logs = Vec::new();
        let mut cumulative = 0u64;
        let mut log_index = 0u32;
        for (index, tx) in plans.iter().enumerate() {
            let (envelope, mut tj) = transaction(number, tx);
            let hash = keccak(&envelope);
            tj["transactionIndex"] = json!(q(index as u64));
            tj["blockNumber"] = json!(q(number));
            let used = 21_000 + 1_000 * index as u64;
            cumulative += used;
            let mut logs = Vec::new();
            let mut logs_json = Vec::new();
            for l in 0..tx.logs {
                let row = LogRow {
                    block_number: number,
                    transaction_index: index as u32,
                    log_index,
                    transaction_hash: hash,
                    address: [0x66; 20],
                    topics: (0..=l).map(|t| [t as u8 + 1; 32]).collect(),
                    data: vec![l as u8; 32],
                };
                logs_json.push(json!({
                    "address": h(&row.address),
                    "topics": row.topics.iter().map(|t| h(t)).collect::<Vec<_>>(),
                    "data": h(&row.data),
                    "logIndex": q(u64::from(log_index)),
                    "removed": false,
                }));
                log_index += 1;
                logs.push(row);
            }
            let bloom = legacy_format::bloom::logs_bloom(&logs).expect("bloom");
            let encoded_logs: Vec<Vec<u8>> = logs
                .iter()
                .map(|l| {
                    let topics: Vec<Vec<u8>> = l.topics.iter().map(|t| rlp(&t[..])).collect();
                    let refs: Vec<&[u8]> = topics.iter().map(Vec::as_slice).collect();
                    list_bytes(&[&rlp(&l.address[..]), &list_bytes(&refs), &rlp(&l.data[..])])
                })
                .collect();
            let log_refs: Vec<&[u8]> = encoded_logs.iter().map(Vec::as_slice).collect();
            let body = list_bytes(&[
                &rlp(&1u8),
                &rlp(&cumulative),
                &rlp(&bloom[..]),
                &list_bytes(&log_refs),
            ]);
            let mut value = Vec::new();
            if tx.kind != 0 {
                value.push(tx.kind);
            }
            value.extend(body);
            receipt_values.push(value);
            let effective = match tx.kind {
                0 | 1 => GAS_PRICE,
                _ => (BASE_FEE + TIP).min(MAX_FEE),
            };
            let mut rj = json!({
                "transactionHash": h(&hash),
                "transactionIndex": q(index as u64),
                "blockNumber": q(number),
                "type": q(u64::from(tx.kind)),
                "status": "0x1",
                "cumulativeGasUsed": q(cumulative),
                "gasUsed": q(used),
                "logsBloom": h(&bloom),
                "logs": logs_json,
                "contractAddress": tx.to.is_none().then(|| h(&create_address(tx.nonce))),
                "effectiveGasPrice": q(effective),
                "from": h(&sender()),
            });
            if tx.kind == 3 {
                rj["blobGasUsed"] = json!("0x20000");
                rj["blobGasPrice"] = json!("0x1");
            }
            receipts_json.push(rj);
            all_logs.extend(logs);
            envelopes.push(envelope);
            txs_json.push(tj);
        }
        let withdrawals: Option<Vec<WithdrawalRow>> = (number >= SHAPELLA).then(|| {
            (0..2)
                .map(|i| {
                    withdrawal_index += 1;
                    WithdrawalRow {
                        block_number: number,
                        index: withdrawal_index - 1,
                        validator_index: 1000 + i,
                        address: [0x77; 20],
                        amount: 32_000 + number % 7,
                    }
                })
                .collect()
        });
        let mut header = HeaderRow {
            block_number: number,
            block_hash: [0; 32],
            parent_hash: parent,
            ommers_hash: keccak(&[0xc0]),
            beneficiary: [0x88; 20],
            state_root: [0x12; 32],
            transactions_root: root(&envelopes),
            receipts_root: root(&receipt_values),
            logs_bloom: legacy_format::bloom::logs_bloom(&all_logs).expect("bloom"),
            difficulty: vec![],
            gas_limit: 30_000_000,
            gas_used: cumulative,
            timestamp: 1_681_000_000 + number,
            extra_data: b"fixture".to_vec(),
            mix_hash: [0x34; 32],
            nonce: [0; 8],
            base_fee_per_gas: Some(rlp_trim(BASE_FEE)),
            withdrawals_root: withdrawals.as_ref().map(|w| {
                legacy_format::ethereum_withdrawals::withdrawal_root(number, w).expect("root")
            }),
            blob_gas_used: (number >= CANCUN).then_some(131_072),
            excess_blob_gas: (number >= CANCUN).then_some(0),
            parent_beacon_block_root: (number >= CANCUN).then_some([0x56; 32]),
            requests_hash: (number >= PRAGUE).then_some([0x78; 32]),
            total_difficulty: None,
        };
        header.block_hash = legacy_format::ethereum::header_hash(&header).expect("hash");
        parent = header.block_hash;
        for tj in &mut txs_json {
            tj["blockHash"] = json!(h(&header.block_hash));
        }
        let mut block = json!({
            "number": q(number),
            "hash": h(&header.block_hash),
            "parentHash": h(&header.parent_hash),
            "sha3Uncles": h(&header.ommers_hash),
            "miner": h(&header.beneficiary),
            "stateRoot": h(&header.state_root),
            "transactionsRoot": h(&header.transactions_root),
            "receiptsRoot": h(&header.receipts_root),
            "logsBloom": h(&header.logs_bloom),
            "difficulty": "0x0",
            "gasLimit": q(header.gas_limit),
            "gasUsed": q(header.gas_used),
            "timestamp": q(header.timestamp),
            "extraData": h(&header.extra_data),
            "mixHash": h(&header.mix_hash),
            "nonce": h(&header.nonce),
            "baseFeePerGas": q(BASE_FEE),
            "uncles": [],
            "size": "0x1",
            "transactions": txs_json,
        });
        if let Some(root) = header.withdrawals_root {
            block["withdrawalsRoot"] = json!(h(&root));
            block["withdrawals"] = json!(withdrawals
                .as_ref()
                .expect("withdrawals with root")
                .iter()
                .map(|w| json!({
                    "index": q(w.index), "validatorIndex": q(w.validator_index),
                    "address": h(&w.address), "amount": q(w.amount)
                }))
                .collect::<Vec<_>>());
        }
        if number >= CANCUN {
            block["blobGasUsed"] = json!(q(131_072));
            block["excessBlobGas"] = json!("0x0");
            block["parentBeaconBlockRoot"] = json!(h(&[0x56; 32]));
        }
        if number >= PRAGUE {
            block["requestsHash"] = json!(h(&[0x78; 32]));
        }
        blocks.push((block, Value::Array(receipts_json)));
    }
    Fixture { blocks }
}

fn rlp_trim(v: u64) -> Vec<u8> {
    v.to_be_bytes()
        .iter()
        .copied()
        .skip_while(|b| *b == 0)
        .collect()
}

/// How the fixture server misbehaves.
#[derive(Default)]
pub struct Knobs {
    pub no_block_receipts: bool,
    pub shuffle_batches: bool,
    pub corrupt_once: HashSet<u64>,
    pub corrupt_always: HashSet<u64>,
    /// Every nth method call gets a -32005 rate-limit error.
    pub transient_every: Option<u64>,
    pub finalized: Option<u64>,
    /// Served block numbers per method, for resume assertions.
    pub served: HashMap<&'static str, Vec<u64>>,
    pub calls: u64,
}

pub struct Server {
    pub url: String,
    pub knobs: Arc<Mutex<Knobs>>,
}

pub fn serve(knobs: Knobs) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/", listener.local_addr().expect("addr"));
    let knobs = Arc::new(Mutex::new(knobs));
    let shared = knobs.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let knobs = shared.clone();
            std::thread::spawn(move || connection(stream, knobs));
        }
    });
    Server { url, knobs }
}

fn connection(stream: std::net::TcpStream, knobs: Arc<Mutex<Knobs>>) {
    let _ = stream.set_nodelay(true);
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    loop {
        let mut length = 0usize;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let request: Value = serde_json::from_slice(&body).expect("json request");
        let reply = {
            let mut knobs = knobs.lock().expect("knobs");
            match request {
                Value::Array(calls) => {
                    let mut replies: Vec<Value> =
                        calls.iter().map(|c| answer(c, &mut knobs)).collect();
                    if knobs.shuffle_batches {
                        replies.reverse();
                        if replies.len() > 2 {
                            replies.swap(0, 1);
                        }
                    }
                    Value::Array(replies)
                }
                call => answer(&call, &mut knobs),
            }
        };
        let text = serde_json::to_vec(&reply).expect("reply");
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
            text.len()
        );
        let mut out = head.into_bytes();
        out.extend(text);
        if writer.write_all(&out).is_err() {
            return;
        }
    }
}

fn answer(call: &Value, knobs: &mut Knobs) -> Value {
    let id = call["id"].clone();
    let method = call["method"].as_str().unwrap_or("");
    let params = &call["params"];
    knobs.calls += 1;
    let error = |code: i64, message: &str| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
    if knobs
        .transient_every
        .is_some_and(|n| knobs.calls.is_multiple_of(n))
    {
        return error(-32005, "limit exceeded");
    }
    let range = range();
    let number = |p: &Value| {
        p.as_str()
            .and_then(|t| u64::from_str_radix(t.trim_start_matches("0x"), 16).ok())
    };
    let result = match method {
        "eth_chainId" => json!("0x1"),
        "eth_getBlockByNumber" if params[0] == "finalized" => {
            json!({"number": q(knobs.finalized.unwrap_or(range.end + 64))})
        }
        "eth_getBlockByNumber" | "eth_getBlockReceipts" => {
            if method == "eth_getBlockReceipts" && knobs.no_block_receipts {
                return error(-32601, "the method eth_getBlockReceipts does not exist");
            }
            let Some(n) = number(&params[0]).filter(|n| *n >= range.start && *n <= range.end)
            else {
                return json!({"jsonrpc": "2.0", "id": id, "result": null});
            };
            let key = if method == "eth_getBlockByNumber" {
                "block"
            } else {
                "receipts"
            };
            knobs.served.entry(key).or_default().push(n);
            let (block, receipts) = &chain().blocks[(n - range.start) as usize];
            let mut value = if key == "block" {
                block.clone()
            } else {
                receipts.clone()
            };
            let corrupt = knobs.corrupt_always.contains(&n)
                || (key == "block" && knobs.corrupt_once.remove(&n));
            if corrupt && key == "block" {
                // A plausible lie: one transaction's value is off by one.
                value["transactions"][0]["value"] = json!("0xdead");
            }
            value
        }
        _ => return error(-32601, "method not found"),
    };
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}
