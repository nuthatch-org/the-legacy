# RFC-0001: The Legacy - A Sealed, Verifiable History Corpus for EVM Chains

- **RFC:** 0001
- **Status:** Draft
- **Author:** Pete (Petko Pavlovski)
- **Date:** 2026-09-14
- **Repo:** github.com/nuthatch-org/the-legacy

**Follow-up:** [RFC-0002](0002-the-backfill-layer.md) proposes sealed-only serving, a native
reader, transaction sidecars, cost-bounded ingestion and a re-sequenced roadmap. Its proposed
interfaces are not implemented by the current binaries; its §8 tracks the amendments required.

---

## 1. Abstract

The Legacy is an open, verifiable, mirrorable "sealed history corpus" for EVM chains plus a thin Rust
serving binary (Solo). Tagline: *"Everything from before, sealed, kept by one small process."* It
stores finalized block history - headers, transactions, receipts, logs, and optionally call traces -
as immutable segments called **relics**, each a set of Apache Parquet files plus a canonical
manifest. Manifests chain together into a **pact**, giving one root hash per chain per height so any
two mirrors can compare state in a single request. Verification (**cleaning**) rebuilds the
transactions/receipts/withdrawals tries and checks them against header fields, and verifies the
header chain back to a trusted checkpoint or (pre-merge) the era1 SSZ accumulator. Solo answers
finalized block/tx/receipt/log/trace JSON-RPC methods from relics via object-storage byte-range
reads, forwards tip/unfinalized requests to a small pruned upstream node, and cleanly rejects
historical-state calls. This RFC specifies the on-disk formats, index sidecars, manifest/pact
algorithms, ingestion pipelines (**Shadow**), the serving architecture, an AI-native MCP/x402
surface, performance targets, and a threat model. Format and protocol details are verified against
primary sources; where a detail could not be verified it is flagged explicitly in the text.

## 2. Motivation

EIP-4444 "sets `HISTORY_PRUNE_EPOCHS` to 82125 epochs (one earth year)," and the community agreed
that from the first phase of history expiry ("drop day," May 1, 2025) execution clients "are no
longer expected to store pre-merge Blocks and Receipts" and may return errors for such requests.
Historical execution data is becoming a second-class citizen of the base protocol precisely as
demand for it (indexers, analytics, AI agents) explodes.

Existing answers are partial. era1/Portal are verifiable but not query-friendly. Reth static files
and Erigon snapshots are client-internal formats, not serving or verification specs. cryo produces
excellent Parquet but has no manifest, verification, or serving story. HyperSync is a closed hosted
service; SQD is a token-gated Parquet lake. The Legacy fills the gap: a spec anyone can mirror,
verify from first principles, and serve behind a standard JSON-RPC endpoint, with the corpus itself
content-addressed so no producer is privileged.

## 3. Goals / Non-Goals

### 3.1 Goals

- A byte-precise, versioned spec for sealed history segments any implementation can produce and any
  mirror can verify independently.
- Content-addressing and a per-chain manifest chain (pact) enabling O(1) mirror comparison and
  O(log n) divergence localization.
- A single small serving binary (Solo) that is a drop-in finalized-history backend behind
  erpc/proxyd.
- Full parity with Geth/Reth semantics for the finalized subset of `eth_getLogs`,
  `eth_getTransactionByHash`, `eth_getTransactionReceipt`, and block/receipt reads.
- Plain-Parquet data usable directly by DuckDB/DataFusion/Polars over HTTP byte-range.
- AI-native discovery, batch, and verify surfaces (MCP), with optional x402 pay-per-call.

### 3.2 Non-Goals

- **State.** No historical state; no `eth_call`/`eth_getStorageAt`/`eth_getBalance`/`eth_getCode` at
  historical blocks; no `debug_traceCall`. These are forwarded or rejected.
- **Tip/consensus.** The Legacy is finalized-only; Solo forwards the unfinalized head to an upstream
  node.
- **Blob data.** EIP-4844 blob sidecars are excluded (§10.4).
- **Opcode-level traces.** Only call traces are in the optional traces tier.

## 4. Terminology

- **The Legacy** - the project, spec, and corpus as a whole.
- **relic** - one sealed immutable segment: a fixed block range stored as Parquet files plus a
  manifest. Sealed only once past finality.
- **pact** - the per-chain manifest chain; each relic manifest commits to the previous relic's hash,
  yielding one root hash per chain per height.
- **cleaning** - verification of a relic against the header chain and the header chain against a
  checkpoint.
- **Solo** - the Rust serving binary (JSON-RPC splitting proxy).
- **Shadow** - the transcoders/ingesters that produce relics.
- **silo** - a chain/corpus instance; mainnet is "Silo 1".

## 5. Relic Geometry: Block-Range Sizing

A relic covers a fixed, contiguous block range. Two candidate sizes were considered:

- **8192 blocks**, aligning to era1's epoch. Per the era1 spec (eth-clients/e2store-format-specs,
  `era1.md`), the accumulator is computed as `hash_tree_root` of an SSZ `List[HeaderRecord, 8192]`
  where `HeaderRecord = {block_hash: Bytes32, total_difficulty: Uint256}`, and "due to the
  accumulator size limit of 8192, the maximum number of blocks in an Era1 batch is also 8192."
  Aligning relics to era1 epochs pre-merge lets a Shadow map a full aligned era1 file to one relic and
  carry the SSZ accumulator root into the manifest as a verifiable boundary artifact.
- **10,000 blocks**, a round decimal boundary matching cryo's common chunking and easy human
  addressing.

**Decision:** relics use **8192 blocks**, both pre- and post-merge, for a single uniform geometry per
chain. Rationale: a uniform power-of-two range reduces the block-number to relic mapping to a shift
(`relic_index = block >> 13`), keeps pre/post-merge tooling identical, and preserves the
full-era1-file-to-one-relic mapping that makes pre-merge cleaning cheap. The 10,000 option is
rejected because it desynchronizes from era1, forcing a Shadow to split/join era1 epochs and
recompute accumulators. Chains without an era1 corpus (most L2s) use the same 8192 geometry. The
block range is recorded explicitly in every manifest (`blocks_per_relic`), so the constant is
spec-versioned and can change for future silos without breaking existing relics.

The [era1 format](https://github.com/eth-clients/e2store-format-specs/blob/main/formats/era1.md)
allows **at most** 8192 records, not necessarily exactly 8192. A partial or unaligned input must
be staged until an entire relic range is available. In particular, the merge-boundary relic
requires both pre-merge and post-merge data; a partial era1 accumulator does not cover that whole
relic. Its header-chain trust needs explicit handling rather than treating it as a full epoch.

## 6. Relic Contents: Parquet Tables

Each relic is a directory of Parquet files, one per table: `headers`, `transactions`, `receipts`,
`logs`, `withdrawals` (post-Shapella), and optionally `traces`. Logs are stored as a **separate flat
table**, not nested inside receipts.

**Why a flat logs table (decision).** `eth_getLogs` is the dominant, most expensive query; it filters
on `address` and `topic0..topic3` and returns individual log records. A flat, columnar logs table
lets the reader (a) build per-column bloom filters and roaring-bitmap sidecars keyed on
`address`/`topics`, (b) prune row groups by min/max on `block_number`, and (c) project only needed
columns. Nesting logs inside a `list<struct>` column of the receipts table would force decoding
entire receipt rows and defeat column-chunk bloom filters on topic columns. Receipts still carry the
fields needed to rebuild the receipts trie (`status`/`post_state`, `cumulative_gas_used`,
`logs_bloom`, `type`); the `logs` table carries `transaction_index`/`log_index` back-references for
reassembly.

### 6.1 Common Parquet writer settings (normative)

All relic Parquet files MUST use the following writer configuration. These settings reduce byte
drift; content identity is defined by canonical rows (§11.1), not by Parquet framing:

- **Compression:** Zstandard, level 3. Rationale: banteg's full cryo extraction "of every block from
  Erigon consumed 430 GB with zstd level 3 compression"; level 3 is the de-facto community setting
  and near the throughput/ratio knee.
- **Row-group size:** 128 MiB of canonical row bytes (§11.1), including presence bytes and length
  prefixes. For transactions, size `raw_envelope` as its actual nullable binary value rather
  than its identity-only null placeholder, so stored envelopes also consume the budget.
  Append a row if it fits; otherwise start a new group. An individual oversized row
  occupies one group. An exact fit stays in the current group. Empty tables have zero groups.
  This is a deterministic uncompressed sizing measure, not Parquet's estimated encoded size.
- **Data-page size:** 1 MiB target, checked in batches of 1024 values; no separate row-count cap.
- **Dictionary encoding:** enabled for low-cardinality byte columns (addresses, topic0, tx type);
  with a 1 MiB dictionary-page limit and fallback to the column's non-dictionary encoding.
- **Parquet writer version:** 2.0 (data page V2), fixed.
- **Statistics:** min/max enabled on all sort-key and numeric columns.
- **No creation timestamps** in Parquet/Arrow key-value metadata; the `created_by` string is pinned
  to `the-legacy/spec-1`.

The initial reference codec pins arrow-rs/Parquet to 59.3.0. For `logs`, dictionaries are enabled
only on `address` and `topic0`; the three numeric columns use `DELTA_BINARY_PACKED`, all remaining
non-dictionary value encodings are `PLAIN`. Native blooms on `address` and `topic0..topic3` use
false-positive probability 0.01 and a configured maximum distinct-value estimate of 100,000 per
column chunk. Page min/max statistics are enabled on every column. This pins the initial writer
profile, not a guarantee of byte identity across future codec versions.

### 6.2 Encoding notes

Parquet 2.0 physical encodings referenced below: `PLAIN`, `RLE_DICTIONARY` (dictionary),
`DELTA_BINARY_PACKED` (delta for sorted/monotonic integers), `DELTA_BYTE_ARRAY`,
`BYTE_STREAM_SPLIT`. All 20-byte addresses and 32-byte hashes are stored as `FIXED_LEN_BYTE_ARRAY`
of the exact length (raw bytes, not `0x` hex strings) to halve storage vs hex and to make
hashing/bloom insertion direct.

### 6.3 `headers` table

| Column | Parquet physical | Logical/Arrow | Nullable | Encoding | Notes |
|---|---|---|---|---|---|
| `block_number` | INT64 | uint64 | no | DELTA_BINARY_PACKED | sort key, monotonic |
| `block_hash` | FIXED_LEN_BYTE_ARRAY(32) | binary | no | PLAIN | keccak256 of header RLP |
| `parent_hash` | FIXED_LEN_BYTE_ARRAY(32) | binary | no | PLAIN | linkage |
| `ommers_hash` | FIXED_LEN_BYTE_ARRAY(32) | binary | no | RLE_DICTIONARY | constant post-merge |
| `beneficiary` | FIXED_LEN_BYTE_ARRAY(20) | binary | no | RLE_DICTIONARY | coinbase |
| `state_root` | FIXED_LEN_BYTE_ARRAY(32) | binary | no | PLAIN | |
| `transactions_root` | FIXED_LEN_BYTE_ARRAY(32) | binary | no | PLAIN | verified in cleaning |
| `receipts_root` | FIXED_LEN_BYTE_ARRAY(32) | binary | no | PLAIN | verified in cleaning |
| `logs_bloom` | FIXED_LEN_BYTE_ARRAY(256) | binary | no | PLAIN | 2048-bit bloom |
| `difficulty` | BYTE_ARRAY | binary (uint256 BE) | no | PLAIN | 0 post-merge |
| `gas_limit` | INT64 | uint64 | no | DELTA_BINARY_PACKED | |
| `gas_used` | INT64 | uint64 | no | DELTA_BINARY_PACKED | |
| `timestamp` | INT64 | uint64 | no | DELTA_BINARY_PACKED | |
| `extra_data` | BYTE_ARRAY | binary | no | PLAIN | variable |
| `mix_hash` | FIXED_LEN_BYTE_ARRAY(32) | binary | no | PLAIN | prev_randao post-merge |
| `nonce` | FIXED_LEN_BYTE_ARRAY(8) | binary | no | PLAIN | PoW nonce |
| `base_fee_per_gas` | BYTE_ARRAY | binary (uint256 BE) | yes | PLAIN | null pre-London |
| `withdrawals_root` | FIXED_LEN_BYTE_ARRAY(32) | binary | yes | PLAIN | null pre-Shapella |
| `blob_gas_used` | INT64 | uint64 | yes | DELTA_BINARY_PACKED | null pre-Cancun |
| `excess_blob_gas` | INT64 | uint64 | yes | DELTA_BINARY_PACKED | null pre-Cancun |
| `parent_beacon_block_root` | FIXED_LEN_BYTE_ARRAY(32) | binary | yes | PLAIN | null pre-Cancun |
| `requests_hash` | FIXED_LEN_BYTE_ARRAY(32) | binary | yes | PLAIN | null pre-Prague (EIP-7685) |
| `total_difficulty` | BYTE_ARRAY | binary (uint256 BE) | yes | PLAIN | carried from era1 pre-merge |

Sort order: `block_number` strictly ascending, with no duplicates. `uint256` fields are stored
as minimal big-endian magnitudes: zero is the empty byte array, while null means absent. RLP
reconstruction must still add the appropriate integer/string prefixes; these are not complete
RLP encodings. Each nullable fork field is preserved independently. Chain-specific fork activation
and combinations are not validated by the row codec, and `extra_data` remains variable-length
rather than imposing Ethereum's limit on every silo.

The v1 writer applies §6.1 with the column encodings shown above. Dictionaries are enabled only
for `ommers_hash` and `beneficiary`, with PLAIN fallback. All columns have page statistics; header
blooms are not specified. Canonical rows follow exactly this column order (§11.1).

When checking a complete relic, the headers table MUST contain exactly one row for every block
in `block_range`. Stored `parent_hash` values must match the preceding stored `block_hash`, and
the first/last row hashes and first parent must match the manifest boundary. This is **stored
consistency**, not cryptographic header verification: §10.5 additionally requires reconstructing
each header's RLP/Keccak hash and authenticating the chain to its trust anchor.

### 6.4 `transactions` table

| Column | Parquet physical | Logical/Arrow | Nullable | Encoding | Notes |
|---|---|---|---|---|---|
| `block_number` | INT64 | uint64 | no | DELTA_BINARY_PACKED | sort key 1 |
| `transaction_index` | INT32 | uint32 | no | DELTA_BINARY_PACKED | sort key 2 |
| `transaction_hash` | FIXED_LEN_BYTE_ARRAY(32) | binary | no | PLAIN | keccak256 of typed envelope |
| `type` | INT32 | uint8 | no | RLE_DICTIONARY | EIP-2718 type (0/1/2/3/4, 0x7E OP deposit) |
| `nonce` | INT64 | uint64 | no | DELTA_BINARY_PACKED | |
| `from` | FIXED_LEN_BYTE_ARRAY(20) | binary | no | RLE_DICTIONARY | recovered sender |
| `to` | FIXED_LEN_BYTE_ARRAY(20) | binary | yes | RLE_DICTIONARY | null = contract creation |
| `value` | BYTE_ARRAY | binary (uint256 BE) | no | PLAIN | |
| `gas_limit` | INT64 | uint64 | no | DELTA_BINARY_PACKED | |
| `gas_price` | BYTE_ARRAY | binary (uint256 BE) | yes | PLAIN | legacy/2930 only |
| `max_fee_per_gas` | BYTE_ARRAY | binary (uint256 BE) | yes | PLAIN | 1559+ |
| `max_priority_fee_per_gas` | BYTE_ARRAY | binary (uint256 BE) | yes | PLAIN | 1559+ |
| `max_fee_per_blob_gas` | BYTE_ARRAY | binary (uint256 BE) | yes | PLAIN | 4844 only |
| `input` | BYTE_ARRAY | binary | no | PLAIN | calldata |
| `access_list` | BYTE_ARRAY | binary (RLP) | yes | PLAIN | 2930+ |
| `blob_versioned_hashes` | BYTE_ARRAY | binary (RLP list) | yes | PLAIN | 4844; hashes only, no blobs |
| `authorization_list` | BYTE_ARRAY | binary (RLP) | yes | PLAIN | 7702 |
| `v_or_y_parity` | INT32 | uint8 | yes | PLAIN | normalized signature value; see below |
| `r` | FIXED_LEN_BYTE_ARRAY(32) | binary | yes | PLAIN | null for OP deposits |
| `s` | FIXED_LEN_BYTE_ARRAY(32) | binary | yes | PLAIN | null for OP deposits |
| `chain_id` | INT64 | uint64 | yes | RLE_DICTIONARY | null for legacy pre-155 |
| `source_hash` | FIXED_LEN_BYTE_ARRAY(32) | binary | yes | PLAIN | OP deposit (0x7E) |
| `mint` | BYTE_ARRAY | binary (uint256 BE) | yes | PLAIN | OP deposit |
| `is_system_tx` | BOOLEAN | bool | yes | RLE | OP deposit |
| `raw_envelope` | BYTE_ARRAY | binary | yes | PLAIN | optional; see below |

Sort order: (`block_number`, `transaction_index`). The typed-envelope bytes are reconstructable from
these columns for trie verification. **Decision:** `raw_envelope` remains a nullable optional
column, defaulting **on** for the archive-RPC and era1 Shadows and **off** for the Reth/Erigon
Shadows to save space. era1 supplies encoded transaction bodies; a JSON-RPC block response may
instead require reconstructing the signed envelope from fields. Stored envelopes must agree with
the structured columns. The current chain ID 1 root checker requires raw envelopes, checks their
Keccak transaction hashes and rebuilds the ordered trie; a future structured-envelope encoder can
extend that check to rows without raw bytes. Raw/structured agreement is not yet checked.

**Signature normalization.** For unprotected legacy transactions (`type = 0`, `chain_id = null`),
`v_or_y_parity` stores 27 or 28. For EIP-155 legacy transactions it stores parity 0 or 1;
reconstruct wire `v` as `2 * chain_id + 35 + parity` using a wide integer, not uint8 or uint64
arithmetic. The chain ID is stored separately. Typed signed transactions likewise store parity
0 or 1. Null remains available for unsigned chain-specific transactions such as OP deposits.
This resolves the ambiguous original uint8 column: a wire EIP-155 `v` can exceed 255 even though
its parity cannot. See [EIP-155](https://eips.ethereum.org/EIPS/eip-155).

**Current codec.** The row and Parquet codecs preserve all 25 columns, enforce minimal uint256
magnitudes, the normalized parity representation and strict `(block_number, transaction_index)`
ordering. They permit slices and do not assert contiguous transaction indices or block completeness.
RLP-valued columns are retained as opaque bytes. Their canonical RLP syntax and semantics,
transaction-type field combinations, signatures, sender recovery, transaction hashes and
raw-envelope agreement are not yet checked. Raw envelopes contribute only the prescribed null
placeholder to `content_hash`, but their actual bytes remain covered by the file hash.
The writer enables dictionaries on `type`, `from`, `to` and `chain_id`, delta encoding on
`block_number`, `transaction_index`, `nonce` and `gas_limit`, and RLE for `is_system_tx`.
Other columns use PLAIN; every column has page statistics. No native transaction blooms are
specified. These are storage/content checks, not transaction or trie verification.

### 6.5 `receipts` table

| Column | Parquet physical | Logical/Arrow | Nullable | Encoding | Notes |
|---|---|---|---|---|---|
| `block_number` | INT64 | uint64 | no | DELTA_BINARY_PACKED | sort key 1 |
| `transaction_index` | INT32 | uint32 | no | DELTA_BINARY_PACKED | sort key 2 |
| `transaction_hash` | FIXED_LEN_BYTE_ARRAY(32) | binary | no | PLAIN | |
| `type` | INT32 | uint8 | no | RLE_DICTIONARY | matches tx type |
| `status` | INT32 | uint8 | yes | RLE_DICTIONARY | post-Byzantium (EIP-658); null pre-Byz |
| `post_state` | FIXED_LEN_BYTE_ARRAY(32) | binary | yes | PLAIN | pre-Byzantium state root; null post-Byz |
| `cumulative_gas_used` | INT64 | uint64 | no | DELTA_BINARY_PACKED | |
| `logs_bloom` | FIXED_LEN_BYTE_ARRAY(256) | binary | no | PLAIN | per-receipt bloom |
| `gas_used` | INT64 | uint64 | yes | DELTA_BINARY_PACKED | derived convenience |
| `contract_address` | FIXED_LEN_BYTE_ARRAY(20) | binary | yes | RLE_DICTIONARY | non-null on creation |
| `effective_gas_price` | BYTE_ARRAY | binary (uint256 BE) | yes | PLAIN | |
| `blob_gas_used` | INT64 | uint64 | yes | DELTA_BINARY_PACKED | 4844 |
| `blob_gas_price` | BYTE_ARRAY | binary (uint256 BE) | yes | PLAIN | 4844 |
| `deposit_nonce` | INT64 | uint64 | yes | DELTA_BINARY_PACKED | OP Regolith+ |
| `deposit_receipt_version` | INT32 | uint8 | yes | RLE_DICTIONARY | OP Canyon+ |
| `l1_fee` | BYTE_ARRAY | binary (uint256 BE) | yes | PLAIN | OP-stack |
| `l1_gas_used` | BYTE_ARRAY | binary (uint256 BE) | yes | PLAIN | OP-stack |
| `l1_gas_price` | BYTE_ARRAY | binary (uint256 BE) | yes | PLAIN | OP-stack |
| `l1_fee_scalar` | BYTE_ARRAY | binary | yes | PLAIN | OP-stack |

Exactly one of `status`/`post_state` is non-null per row: pre-Byzantium receipts carry a 32-byte
intermediate state root; EIP-658 (Byzantium) replaced it with a boolean status. This distinction is
essential to rebuild the receipts trie leaf bytes correctly (§10.2). A present status MUST be
0 or 1, as defined by [EIP-658](https://eips.ethereum.org/EIPS/eip-658).

Sort order: (`block_number`, `transaction_index`), strictly increasing with no duplicate keys.
The codec permits slices, so it does not require contiguous transaction indices or prove that
all transactions have receipts. All uint256 columns use minimal big-endian magnitudes as in
§11.1; present empty bytes mean zero, and null means absent. `l1_fee_scalar` is opaque binary,
not a uint256, and the codec preserves its bytes without interpretation or normalization.

The v1 writer follows §6.1. Dictionaries are enabled on `type`, `status`, `contract_address`
and `deposit_receipt_version`, with PLAIN fallback. Delta encoding applies to the integer
columns marked above; remaining columns use PLAIN. All columns have page statistics and no
native receipt blooms are specified. The original bare RLE entry for the integer
`deposit_receipt_version` was invalid: [Parquet's RLE value encoding](https://parquet.apache.org/docs/file-format/data-pages/encodings/)
applies to booleans and dictionary indices, not plain INT32 values.

The implemented codec checks these row invariants and preserves all 19 columns. It does not
validate fork activation, chain-specific field combinations, transaction/log correspondence,
bloom contents, cumulative/derived gas fields, fee calculations or receipts trie roots. These
require later chain-aware or cross-table verification; a matching content hash is not a claim
that the receipt describes a real transaction.

### 6.6 `logs` table

| Column | Parquet physical | Logical/Arrow | Nullable | Encoding | Notes |
|---|---|---|---|---|---|
| `block_number` | INT64 | uint64 | no | DELTA_BINARY_PACKED | sort key 1 |
| `transaction_index` | INT32 | uint32 | no | DELTA_BINARY_PACKED | sort key 2 |
| `log_index` | INT32 | uint32 | no | DELTA_BINARY_PACKED | block-level index; sort key 3 |
| `transaction_hash` | FIXED_LEN_BYTE_ARRAY(32) | binary | no | PLAIN | |
| `address` | FIXED_LEN_BYTE_ARRAY(20) | binary | no | RLE_DICTIONARY | **bloom + bitmap** |
| `topic0` | FIXED_LEN_BYTE_ARRAY(32) | binary | yes | RLE_DICTIONARY | event sig; **bloom + bitmap** |
| `topic1` | FIXED_LEN_BYTE_ARRAY(32) | binary | yes | PLAIN | **bloom** |
| `topic2` | FIXED_LEN_BYTE_ARRAY(32) | binary | yes | PLAIN | **bloom** |
| `topic3` | FIXED_LEN_BYTE_ARRAY(32) | binary | yes | PLAIN | **bloom** |
| `data` | BYTE_ARRAY | binary | no | PLAIN | unindexed event data |

Sort order: (`block_number`, `transaction_index`, `log_index`), strictly increasing with no duplicate
keys. Within a block, `log_index` strictly increases even across transaction boundaries. Topics
form a contiguous prefix of zero to four values: a non-null topic cannot follow a null topic.
A present all-zero topic is distinct from an absent topic. Per the JSON-RPC spec a log object
has a `removed` field; it is **never** stored: relics are finalized-only, so every log is canonical
and Solo always serializes `removed: false`.

### 6.7 `withdrawals` table (post-Shapella)

| Column | Parquet physical | Logical/Arrow | Nullable | Notes |
|---|---|---|---|---|
| `block_number` | INT64 | uint64 | no | sort key 1 |
| `index` | INT64 | uint64 | no | global withdrawal index; sort key 2 |
| `validator_index` | INT64 | uint64 | no | |
| `address` | FIXED_LEN_BYTE_ARRAY(20) | binary | no | recipient |
| `amount` | INT64 | uint64 | no | Gwei |

Rows are strictly ordered by (`block_number`, `index`), with the global `index` also strictly
increasing across blocks. The row codec checks monotonicity but does not infer completeness from
it. Under [EIP-4895](https://eips.ethereum.org/EIPS/eip-4895), `amount` is nonzero and remains in
Gwei; the global index is distinct from the withdrawal's position used as its per-block trie key.
An empty table is valid. Determining whether the table is required at a particular fork, whether
withdrawals are missing, and whether they match the header root requires later chain checks.

The v1 withdrawals writer uses the shared §6.1 profile, `DELTA_BINARY_PACKED` for all four uint64
columns and dictionary encoding for `address` (PLAIN fallback). All columns have page statistics;
no withdrawal bloom filters are specified yet. Each canonical row is exactly 57 bytes, including
presence markers, so the row-group sizing rule applies without an encoded-size estimate.

### 6.8 `traces` table (optional tier)

| Column | Parquet physical | Logical/Arrow | Nullable | Notes |
|---|---|---|---|---|
| `block_number` | INT64 | uint64 | no | sort key 1 |
| `transaction_index` | INT32 | uint32 | yes | null for block/reward traces |
| `trace_address` | BYTE_ARRAY | binary (RLP list of ints) | no | position in call tree |
| `type` | INT32 | uint8 | no | call/create/suicide/reward |
| `call_type` | INT32 | uint8 | yes | call/callcode/delegatecall/staticcall |
| `from` | FIXED_LEN_BYTE_ARRAY(20) | binary | yes | |
| `to` | FIXED_LEN_BYTE_ARRAY(20) | binary | yes | |
| `value` | BYTE_ARRAY | binary (uint256 BE) | yes | |
| `gas` | INT64 | uint64 | yes | |
| `gas_used` | INT64 | uint64 | yes | |
| `input` | BYTE_ARRAY | binary | yes | |
| `output` | BYTE_ARRAY | binary | yes | |
| `error` | BYTE_ARRAY | binary | yes | |

**Traces are not header-committed.** No field in the Ethereum block header commits to call traces.
Consequently traces CANNOT be verified against the header chain. They are verified only by (a)
cross-producer agreement - two independent Shadows over different sources (e.g. Reth re-execution vs
Firehose call trees) producing the same canonical row-hash - or (b) local re-execution. This gap is
stated plainly and is not papered over by any cryptographic claim in the manifest.

## 7. Index Sidecars

All sidecars are **rebuildable by any mirror from the Parquet data alone**, so no producer is
privileged. Their hashes are recorded in the manifest for integrity, but a mirror may recompute and
check them.

### 7.1 Log address/topic bitmaps

For the `logs` table, per relic: roaring bitmaps mapping `address` to a set of row-group indices,
`address` to a set of row indices, and `topic0..topic3` to sets of row-group indices. Serialization
uses the **RoaringBitmap portable format** (RoaringFormatSpec): little-endian words, the
`SERIAL_COOKIE_NO_RUNCONTAINER` cookie (or the run-container cookie), 32-bit containers keyed by the
high 16 bits. This format is interoperable across the Rust `roaring`, Java, Go, and C/CRoaring
implementations, so a mirror in any language reads them. The sidecar container is a small custom
file: a fixed magic, a directory of (key to offset, length) sorted by key, then concatenated
portable bitmaps; keys are the raw 20-byte address or 32-byte topic. Lookup: binary-search the
directory, deserialize the bitmap, intersect across topic positions and with the block-range bitmap.

### 7.2 Transaction-hash sidecar

Per relic: a sorted array of fixed-width (`transaction_hash`, `block_number`, `transaction_index`)
records (44 bytes each: 32+8+4) sorted by hash, plus a **Split Block Bloom Filter (SBBF)** over the
hashes for negative lookups. Per the Parquet bloom spec the SBBF uses eight hash functions, 256-bit
(32-byte) blocks, and XXH64 with seed 0; sizing follows the Parquet formula - e.g. 1024 blocks /
32 KB gives approximately 1.26% FPP at around 26k values. Lookup: check the SBBF; on a hit,
binary-search the sorted array.

### 7.3 Block-hash to block-number sidecar

Per relic: a sorted array of (`block_hash`, `block_number`) records (40 bytes each) plus an SBBF,
same lookup discipline.

### 7.4 Native Parquet blooms vs external sidecars (decision)

Parquet's native column-chunk SBBFs store per-row-group filter offset/length in row-group metadata
and are the only bloom representation Parquet supports. **Decision:** use **both**, for different
jobs. Native column-chunk blooms are enabled on `logs.address`, `logs.topic0`, and
`transactions.transaction_hash` so any Parquet reader - DuckDB, DataFusion, Polars - gets row-group
pruning for free with zero external state. The **external roaring sidecars are Solo's primary
path**, because they map a key to exact rows across the whole relic in one small read, whereas
native blooms require reading each row group's footer to test membership. Sidecars win for Solo's
latency budget; native blooms win for third-party SQL portability. Both are cheap; storing both is
justified.

## 8. Manifest and Pact

### 8.1 Format decision

The manifest is **canonical JSON** (RFC 8785 JSON Canonicalization Scheme, JCS): keys sorted, no
insignificant whitespace, UTF-8, canonical number forms. Rationale: JSON is universally tooled and
human-auditable; JCS makes it canonical-encodable so its hash is stable across producers. TOML was
rejected - no standardized canonical form and ambiguous typing for byte strings. CBOR was rejected -
although canonical CBOR (RFC 8949 §4.2) exists and is compact, it is not human-auditable and the
size win is negligible for a per-relic document. Byte values are lowercase hex without `0x` in a JCS
string.

### 8.2 Hash decision

All content hashes (per-file, manifest, pact) use **BLAKE3**. Rationale: BLAKE3 is dramatically
faster than SHA-256 (decisive when a mirror re-hashes hundreds of GB to verify), is parallelizable
and streamable, and its 256-bit output is collision-resistant for content-addressing. SHA-256 was
considered for ubiquity but its bulk-verify throughput penalty dominates. keccak256 is not used for
file hashing (slower, no advantage outside trie work). The manifest records `hash_algo: "blake3"` so
a future spec version can migrate.

### 8.3 Manifest schema (fields)

- `spec_version` (u32)
- `chain_id` (u64)
- `silo` (string, e.g. `"Silo 1"`)
- `block_range` = `{ start, end }` (inclusive, u64)
- `boundary` = `{ start_block_hash, end_block_hash, parent_hash_of_start }` (hex32)
- `era1_accumulator_root` (hex32, optional; present for pre-merge relics)
- `blocks_per_relic` (u64, = 8192)
- `files`: array of `{ name, table, byte_size, blake3, content_hash, row_count, row_groups }`
- `producer`: optional `{ identity, scheme, signature }`, `scheme` in
  `{"ethereum-secp256k1","ed25519"}`, signature over the manifest hash
- `prev_relic_manifest_hash` (hex32; all-zero for the genesis relic)
- `pact_root` (hex32; §8.4)
- `hash_algo` (string, `"blake3"`)

Each v1 data-table `name` MUST be exactly `<table>.parquet` from §6, not an arbitrary path. There
is one file per table and duplicate names are invalid. This also prevents a local cleaner from
following manifest-supplied absolute paths or parent-directory traversal. Future sidecar entries
need their own schema definition; see RFC-0002 P3.

### 8.4 Pact root algorithm

The pact root is a running hash chain over relic manifests:

```rust
// manifest_hash_n = BLAKE3 over the JCS-canonical manifest bytes
// with the `pact_root` field set to the all-zero 32-byte value.
fn manifest_hash(manifest_jcs_with_zero_pact_root: &[u8]) -> [u8; 32] {
    blake3::hash(manifest_jcs_with_zero_pact_root).into()
}

// pact_root_0 = manifest_hash_0            (genesis relic: prev = 0x00..00)
// pact_root_n = BLAKE3( pact_root_{n-1} || manifest_hash_n )
fn pact_root(prev_pact_root: [u8; 32], manifest_hash_n: [u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&prev_pact_root);
    h.update(&manifest_hash_n);
    *h.finalize().as_bytes()
}
```

The computed `pact_root` is written back into the manifest's `pact_root` field, and
`prev_relic_manifest_hash` is set to `manifest_hash_{n-1}`. Two mirrors at the same height compare a
single 32-byte `pact_root`; if equal, every relic and every file byte-for-byte agrees (by BLAKE3
collision resistance and the hash-chain construction). If they differ, a binary search over relic
manifest hashes localizes the first divergent relic in O(log n) requests.

This compares mirrors of an **exact manifest chain**, not arbitrary independent producers.
Different Parquet bytes or producer metadata change the manifest and pact even when canonical
table contents agree. Cross-producer conformance compares `content_hash` for matching chain,
schema version, relic range and table (§11.1); it does not require matching pact roots.

### 8.5 Registry document

A per-chain `registry.json` (JCS) lists: `chain_id`, `spec_version`, `head_pact_root`, `head_block`,
an array of relics `{ index, block_range, manifest_url, pact_root }`, and a `mirrors` array
`{ name, base_url, transport }` with `transport` in `{"https","s3","torrent"}`. A new mirror
bootstraps by fetching the registry, then for each relic fetching the manifest, verifying file
BLAKE3 hashes and the pact chain, and cleaning (§10).

### 8.6 Example manifest

```json
{
  "block_range": { "end": 20086783, "start": 20078592 },
  "blocks_per_relic": 8192,
  "boundary": {
    "end_block_hash": "b1…",
    "parent_hash_of_start": "9c…",
    "start_block_hash": "a0…"
  },
  "chain_id": 1,
  "files": [
    { "blake3": "d4…", "byte_size": 41231872, "content_hash": "77…",
      "name": "logs.parquet", "row_count": 812344, "row_groups": 3, "table": "logs" }
  ],
  "hash_algo": "blake3",
  "pact_root": "e9…",
  "prev_relic_manifest_hash": "c2…",
  "producer": { "identity": "0xPete…", "scheme": "ethereum-secp256k1", "signature": "30…" },
  "silo": "Silo 1",
  "spec_version": 1
}
```

## 9. Object-Storage Layout and Distribution

### 9.1 Path convention

```
s3://<bucket>/legacy/v<spec_version>/<chain_id>/relics/<relic_index_padded>/
    manifest.json
    headers.parquet
    transactions.parquet
    receipts.parquet
    logs.parquet
    withdrawals.parquet          # post-Shapella
    traces.parquet               # optional tier
    idx/logs.addr.roaring
    idx/logs.topics.roaring
    idx/txhash.sidecar
    idx/blockhash.sidecar
legacy/v<spec_version>/<chain_id>/registry.json
```

`relic_index_padded` is the zero-padded relic index (`block >> 13`). Files are content-addressed via
manifest BLAKE3; a mirror MAY additionally store objects under a `by-hash/<blake3>` prefix to dedup
identical files across silos/versions.

### 9.2 Cost and size estimates (flagged as estimates)

- **Corpus size.** banteg's own figures: the `cryo logs` dataset is "125 GB" and the
  `cryo transactions` dataset is "304 GB" (banteg.xyz, "Ethereum Transfers Heatmap"); banteg's
  Twitter report (quoted via degencode.com) is that "a full Cryo extraction of every block from
  Erigon consumed 430 GB with zstd level 3 compression." The Legacy's mainnet
  headers+transactions+receipts+logs corpus is therefore estimated at **approximately 400 to
  500 GB** in zstd Parquet at 2023-era heights, growing with the chain. *(Estimate.)*
- **Traces tier.** Traces are a separate **multi-TB** tier for mainnet; full-chain trace extraction
  is a multi-hour job on an archive node. *(Estimate; a specific "around 9 hours on Erigon" figure
  appears in banteg's write-up but could not be independently corroborated and is not relied upon
  here.)*
- **L2 reality.** High-throughput L2s (Arbitrum, Optimism, Base) have far more transactions than
  mainnet, so their corpora are **materially larger** than mainnet's - plan for multiples, not
  fractions. This is the expensive case, not the cheap one, and is stated plainly. *(Estimate.)*
- **Storage cost.** Per Cloudflare's official pricing (verified Sept 2026): R2 storage is
  **$0.015/GB-month**, Class A operations **$4.50/million**, Class B operations **$0.36/million**,
  **egress free**, with a free tier of 10 GB storage / 1M Class A / 10M Class B per month. AWS S3
  Standard is approximately $0.023/GB-month with approximately $0.09/GB egress. For a read-heavy
  public corpus, R2's zero egress is decisive: 500 GB on R2 is approximately **$7.50/month** storage
  with no egress bill; the same on S3 incurs egress on every download. *(R2/S3 rates cited; totals
  are estimates.)*

Distribution is by HTTP(S) byte-range from object storage, optionally BitTorrent (Erigon's
precedent: `.seg` snapshots are content-addressed and BitTorrent-distributed), with the registry as
the entry point.

## 10. Verification (Cleaning)

Cleaning has two independent halves: relic-contents verification and header-chain verification.

### 10.1 Transactions trie

Rebuild the Merkle-Patricia trie `Trie(rlp(index) => value)` for `index` in `0..n`, where `value` is
the typed transaction envelope per EIP-2718: `TransactionType || TransactionPayload` for typed
transactions, or the raw RLP list for legacy (the first byte distinguishes them - typed transactions
begin with a value in `[0, 0x7f]`, legacy with `>= 0xc0`). The trie key is `RLP(index)`. Compare
`trie.root()` against `headers.transactions_root`. Solo/Shadow use `alloy`/`reth-primitives`
encoders (`ordered_trie_root_with_encoder` computes exactly this).

For chain ID 1, the current cleaner does this only when every transaction has `raw_envelope`.
It accepts legacy envelopes beginning with an RLP list and types 1 through 4 with the matching
raw prefix and an RLP-list payload. It checks `keccak256(raw_envelope) == transaction_hash`,
requires indices contiguous from zero per block, and compares the resulting ordered trie root to
`headers.transactions_root`. Empty blocks use the Ethereum empty trie root. It does not decode
the individual transaction fields, establish raw/structured agreement, recover senders, validate
signatures or apply a fork schedule. Missing headers or transactions report **not checked**;
other chains report an unsupported transaction profile.

### 10.2 Receipts trie

For Ethereum, [EIP-2718](https://eips.ethereum.org/EIPS/eip-2718) defines trie keys as
`RLP(transaction_index)`. A legacy receipt value is the bare
`RLP([outcome, cumulativeGasUsed, logsBloom, logs])`, with **no zero type prefix**. Types 1
through 4 prepend their single raw type byte to that RLP list, without an outer RLP string
wrapper. The original instruction to preserve a possible zero-prefixed legacy form was incorrect
for this Ethereum profile and could produce the wrong root.

The outcome is the status integer after [EIP-658](https://eips.ethereum.org/EIPS/eip-658), or a
32-byte intermediate state root for pre-Byzantium legacy receipts. Status zero is the empty RLP
string (`80`), not the single byte `00`; status one encodes as `01`. Each log is
`RLP([address, [topic0, ...], data])` in log order. Transaction hashes, block numbers, gas_used,
fees, contract addresses and other convenience columns are excluded from the receipt value.

`legacy-format::ethereum_receipts` implements these key/value bytes for legacy and types 1..=4.
It validates row shapes and log order/key/hash references, rejects typed state-root receipts and
unsupported types (including OP deposits), and encodes the supplied receipt bloom as stored.
Bloom reconstruction remains a separate check; an empty log slice is not proof that no logs
were omitted. This API is explicitly Ethereum-specific, not an automatic profile for every silo.
Fork activation, complete receipt/log sets and trie construction are not implemented by the
encoder. Independent synthetic RLP vectors and their Python generator are checked in under
`crates/legacy-format/tests/fixtures/`; they are not authenticated chain fixtures.

For chain ID 1, cleaning rebuilds the ordered Merkle-Patricia trie from those values and compares
the root against `headers.receipts_root`. It requires headers, receipts and logs to be present,
and requires receipt indices to be contiguous from zero within every block. Empty blocks use the
Ethereum empty trie root. The implementation uses a pre-encoded ordered trie builder, so receipt
values are inserted without an extra RLP string wrapper. It is tested against an independent,
small recursive MPT oracle across receipt-index boundaries 127, 128 and 256.

The result is reported as `receipts_root`. It proves that the supplied receipt rows and log bytes
produce the supplied header field, not that the header belongs to the canonical chain. It does
not validate transaction trie values, execution, receipt completeness, fork activation or a
checkpoint. Other chains report an unsupported receipt profile. Missing any required table
reports **not checked**, rather than assuming an empty one.

### 10.3 Withdrawals root

For post-Shapella blocks (EIP-4895), rebuild the withdrawals trie identically -
`compute_trie_root_from_indexed_data(withdrawals)`, "constructed identically to the transactions
root … by inserting each withdrawal into a Merkle-Patricia trie keyed by index" - and compare
against `headers.withdrawals_root`. Withdrawals live in the per-relic `withdrawals` table (§6.7).

For chain ID 1, the current cleaner encodes each value as
`RLP([global_index, validator_index, address, amount])` and uses its zero-based position within
the block as the ordered trie key. The global index is therefore never mistaken for the key.
It requires headers and withdrawals, compares each header with a `withdrawals_root` against the
rebuilt root, and rejects supplied withdrawals under a header that has no root. Empty
post-Shapella blocks use the Ethereum empty trie root. It does not apply a fork schedule, prove
table completeness or authenticate the header. Missing tables report **not checked** and other
chains report an unsupported withdrawal profile.

### 10.4 Blob transactions (EIP-4844)

Blob transactions (type 0x03) are stored in **canonical form** - the `tx_payload_body` fields plus
`blob_versioned_hashes` - exactly as committed by `transactions_root`. The **network form** (which
additionally wraps blobs, KZG commitments, and KZG proofs, as required by `eth_sendRawTransaction`)
is **not** stored: each blob is 128 KB (131,072 bytes) with a 48-byte commitment and 48-byte proof,
is a consensus-layer sidecar "propagated separately," and is retained only
"MIN_EPOCHS_FOR_BLOB_SIDECARS_REQUESTS epochs, which is around 18 days" - never committed by the
execution header. Cleaning verifies the canonical form against `transactions_root`; it makes no
claim about blob contents. This exclusion is deliberate and stated plainly.

### 10.5 Header chain

- **Header hash:** reconstruct each execution header using its chain/fork-specific fields and
  require `keccak256(header_rlp) == block_hash` before treating stored links as authenticated
  commitments. Comparing claimed hash columns alone is insufficient.
- **Linkage:** verify `headers[i].parent_hash == headers[i-1].block_hash` within and across relics
  (`boundary.parent_hash_of_start` links to the previous relic's `end_block_hash`).
- **Pre-merge:** reconstruct the era1 SSZ accumulator (`hash_tree_root(List[HeaderRecord, 8192])`,
  `HeaderRecord = {block_hash, total_difficulty}`) and compare against `era1_accumulator_root`,
  itself checkable against published era1 files.
- **Post-merge:** anchor the newest verified header to a **trusted checkpoint** - a beacon-chain
  finalized / weak-subjectivity checkpoint (the same anchor EIP-4444 mandates for "checkpoint sync")
  from a configured checkpoint list, or the operator's own node's finalized head. The `parent_hash`
  chain then extends trust backward from that anchor.

### 10.6 Example cleaning report

```json
{
  "chain_id": 1,
  "relic_index": 2451,
  "block_range": {"start": 20078592, "end": 20086783},
  "checks": {
    "file_hashes": "pass",
    "header_linkage": "pass",
    "transactions_root": "pass",
    "receipts_root": "pass",
    "withdrawals_root": "pass",
    "era1_accumulator": "n/a (post-merge)",
    "checkpoint_anchor": {"status": "pass", "source": "beacon-finalized:0xabc…"},
    "index_sidecars": "rebuilt-and-matched",
    "traces": "unverified (not header-committed)"
  },
  "pact_root": "e9…",
  "cleaned_by": "solo/0.1.0",
  "cleaned_at_block": 20090000
}
```

**Verifiable:** header chain, transactions/receipts/withdrawals roots, file integrity, index
correctness. **Not verifiable:** traces (§6.8); `removed` logs (never present - relics are
finalized-only).

### 10.7 Current local cleaning implementation

`solo clean [--json] <manifest>...` checks manifest structure, relic boundary linkage and the
pact chain. `--files` additionally resolves each canonical table name beside its manifest and
checks byte size, file BLAKE3, footer row count, row-group count and the sum of row-group rows.
It rejects unsupported table schema versions, missing files, symlinks and non-regular files.
Hashing and decoding use the same in-memory bytes. No endpoint is contacted.

For `headers`, `transactions`, `receipts`, `logs` and `withdrawals`, file mode also decodes the
table, checks schema/row invariants,
recomputes `content_hash`, checks the decoded count and requires every row's block to fall inside
the relic range. Other tables report these decoded checks as **not checked**; a valid footer or
file hash is not a schema or chain check. Aggregate content status is partial only if some table
contents were actually checked, and unchecked if none were. A failure exits nonzero without
printing a success report. Success is limited to the explicitly reported checks.

Headers additionally check complete block coverage, stored parent links and manifest boundary
agreement. These are reported as `header_coverage`, `stored_header_linkage` and
`manifest_boundary`. For chain ID 1, the cleaner also reconstructs Ethereum header RLP and
requires its Keccak-256 digest to equal `block_hash`. The supported layouts are pre-London (15
fields), London (16), Shanghai (17), Cancun (20) and Prague (21). Optional extensions must be a
contiguous prefix; the three Cancun fields must occur together. `block_hash` and
`total_difficulty` are not part of the preimage. Non-minimal uint256 values are rejected,
including leading-zero magnitudes; integer zero is empty, while the nonce remains eight bytes.
The field order follows [EIP-4788](https://eips.ethereum.org/EIPS/eip-4788) with the
[EIP-7685](https://eips.ethereum.org/EIPS/eip-7685) requests hash appended for Prague.

`header_hashes` and `header_linkage` report **pass** only for requested files checked with this
profile. Other chain IDs report **not checked (unsupported chain header profile)**. The generic
row codec retains its chain-neutral rules. Future header fields beyond the v1 schema are not
supported. Layout is selected by field presence, not by the mainnet fork activation schedule;
`consensus_rules` explicitly reports that schedule and other consensus rules as unchecked.
Hash reconstruction proves consistency of these preimages and their links, not that they belong
to Ethereum's canonical chain. A synthetic hash-consistent chain still passes without a trusted
checkpoint or finality check. An era1 accumulator root is checked only when the manifest
carries one.

When `era1_accumulator_root` is present and headers were read, the cleaner recomputes
`hash_tree_root` of `List[HeaderRecord, 8192]` from each header's `block_hash` and
`total_difficulty`. The row stores total difficulty as a minimal big-endian magnitude; the SSZ
uint256 is 32-byte little-endian. A header with no total difficulty fails the run. A manifest
without the root reports **not checked**. This does not reconstruct header RLP, apply a merge
schedule, or prove the records are a published era1 accumulator. It checks the stored pairs
against the root the manifest claims. Header-hash reconstruction remains the separate chain
ID 1 check.

`--after <manifest>` provides predecessor pact context for a continuation. Its table files are
not checked unless they are part of the requested manifest run. The report names that scope.
Transaction envelope semantics (including raw/structured agreement and transaction hashes) and
signature validity/sender recovery are separately reported as **not checked**. Receipt fees
and other derived fields still report **not checked** under `receipt_consistency`.
Execution gas accounting has its own `receipt_gas` result.
Other table completeness, consensus rules, finality, index correctness,
producer signatures and checkpoint anchoring remain unimplemented. The era1 accumulator
check above runs only when the manifest carries a root. Transaction-root checking
currently requires raw envelopes, and receipt and withdrawal-root checking have the chain ID 1
profiles described above. The example in §10.6 is the intended full report, not current executable
output.
After the file checks, the cleaner compares available tables within each requested relic:

- `transaction_receipt_links`: transactions and receipts must have equal row counts and match
  exactly by `(block_number, transaction_index)`, `transaction_hash` and `type`.
- `log_transaction_links`: each log must match a transaction by block/index and transaction hash.
- `log_receipt_links`: each log must match a receipt by block/index and transaction hash.
- `receipt_blooms`: reconstruct each receipt's 256-byte Ethereum bloom from its supplied logs'
  addresses and topics, and require exact equality, including all-zero blooms for receipts
  without logs. Log references must pass first. For each address/topic, take the first three
  big-endian 16-bit pairs of Keccak-256, mask each to 11 bits, and set that bit in the bloom's
  big-endian byte representation. See [Geth's reference implementation](https://github.com/ethereum/go-ethereum/blob/master/core/types/bloom9.go).

- `header_blooms`: OR all supplied receipt blooms within each block and require exact equality
  with that header's `logs_bloom`. Blocks with no supplied receipts must have an all-zero bloom.
  Every supplied receipt must reference a supplied header. This checks aggregation, not receipt
  bloom reconstruction: it runs even if logs are absent, in which case `receipt_blooms` remains
  unchecked. Header hashes and checkpoint trust retain their separate reported status.

- `receipt_gas` (chain ID 1 only): receipt indices must start at zero and be contiguous within
  each supplied block. Cumulative execution gas must not decrease. Present `gas_used` must equal
  the difference from the previous cumulative total, starting from zero per block; null remains
  absent and is not materialized. The last cumulative total (zero without receipts) must equal
  header `gas_used`, and header `gas_used` must not exceed `gas_limit`. Every receipt must have
  a supplied header. Subtraction is checked rather than wrapping. This is arithmetic consistency,
  not execution validation: zero deltas, transaction gas limits, intrinsic gas, fee calculations
  and blob gas rules are not validated. Other chain IDs report an unsupported gas profile.

Each check runs when its tables and any stated chain profile are available, independently of
the other checks. A missing table reports **not checked**, never an assumed empty table. Present empty tables may
pass. JSON `relic_checks` and the prose report name each relic's results. Aggregate status is
**pass** only when every requested relic passed that check, **partial** when some passed and
others lacked prerequisites, and **not checked** when none ran. Manifest-only mode checks none of these.
A mismatch fails the whole run before any success report is printed.

These checks prove agreement among supplied rows, not completeness, transaction authenticity or
correct execution. Deleting a transaction and its receipt together (and any associated logs)
can still pass. The generic table codecs permit slices; only the Ethereum whole-block gas
check additionally requires contiguous receipt indices. Blooms do not cover log data,
ordering or multiplicity, and collisions can conceal changes to addresses/topics. Matching a
bloom does not prove log completeness. Neither comparison authenticates the header or receipt
set. Fees and other derived fields, log payload correctness and trie roots remain unchecked. The typed table codecs remain usable
on independent slices; these relationships are enforced by the cleaner when counterparts exist.

The file checker retains decoded headers, transactions, receipts and logs for one relic until
its relationship and bloom checks finish, alongside the current file buffer and decoder working data. The relational checks
use the rows decoded from the same bytes checked for file/content hashes, without reopening the
files. This is not a streaming scanner and its memory use can be substantial on busy relics.

## 11. Ingestion (Shadow)

Six Shadow sources, all producing the same relic format:

1. **Archive JSON-RPC** - parallel backfill via `eth_getBlockByNumber`,
   `eth_getBlockReceipts`/`eth_getTransactionReceipt`, `eth_getLogs`. Bootstrapping only (slow,
   rate-limited). Stores `raw_envelope`, reconstructing it from response fields when necessary
   (§6.4). RFC-0002 P4 proposes a cost-bounded first-class producer for silos without local sources.
2. **Reth static files** - read the `headers`, `transactions`, `receipts` NippyJar segments
   directly. Reth static files are immutable NippyJar files (a bincode-serialized config sidecar
   accompanies each) organized by block range; blocks-per-file is configurable
   (`--static-files.blocks-per-file.{headers,transactions,receipts}`).
3. **Erigon 3 snapshots** - read `.seg` block snapshot files (seg-compressed word streams; `.idx`
   accessors map block to offset). Note: Erigon's `.kv/.bt/.kvei` triples are *state-domain* files
   and are out of scope; The Legacy consumes only the block/txn/receipt `.seg` snapshots.
4. **era1** - one full aligned era1 file per pre-merge relic; partial inputs are staged (§5).
   Per the era1 spec each block-tuple is
   `CompressedHeader | CompressedBody | CompressedReceipts | TotalDifficulty` (snappy-framed RLP),
   followed by the SSZ `Accumulator` and `BlockIndex`; it is used as a verifiable input, and its
   accumulator root is carried into the manifest.
5. **Firehose merged blocks** - `pbbstream.Block`-wrapped chain-specific blocks (e.g.
   `sf.ethereum.type.v2.Block`) in merged-block files include call trees, so this is the **traces**
   source. Merged-block files are ordered and deduplicated by the merger.
6. **Reth ExEx** - an Execution Extension consuming the reorg-aware `ExExNotification` stream
   (`ChainCommitted` / `ChainReverted` / `ChainReorged` variants), emitting
   `ExExEvent::FinishedHeight`, and sealing a relic when a range passes finality. This is the
   continuous, steady-state producer.

### 11.1 Determinism

Two independent Shadows over the same source MUST produce **content-identical** relics.
Byte-identical Parquet across arrow-rs versions is impractical (encoder internals, dictionary
ordering, and page boundaries drift between library versions), so the spec defines identity at the
**canonical row-hash** level, not the file-byte level:

- Rows are emitted in the normative sort order (§6) - deterministic and source-independent.
- A table's **content hash** is `BLAKE3` over the concatenation of canonical row encodings defined
  below, with no prefix, row count, delimiter, or suffix. The empty table hashes the empty byte
  string. Compare these hashes only within the same chain, schema version, relic range and table.
- The manifest records both the per-file `blake3` (byte-level integrity of *this* copy) and a
  per-file `content_hash` (framing-independent, for cross-producer agreement).
- Writer settings (§6.1) are pinned to minimize byte drift, so two Shadows on the same arrow-rs
  version SHOULD also achieve byte-identity; conformance requires only content-identity.

This makes cross-producer verification (Reth vs Firehose vs Erigon) meaningful even when Parquet
bytes differ, and is the only verification available for the traces tier.

**Canonical row encoding, v1.** Fields appear in the column order of the corresponding §6 table.
Every field, including required fields, begins with one presence byte: `00` for null (no payload),
or `01` for present (followed by the payload). Null in a required field is invalid. Schema types
and widths are external to the byte stream; there are no type tags or field names.

| Logical type | Present payload, after `01` |
|---|---|
| uint8 / uint32 / uint64 | Exactly 1 / 4 / 8 bytes, unsigned, big-endian |
| bool | Exactly one byte: `00` for false, `01` for true |
| fixed binary(N) | Exactly N raw bytes |
| binary, including opaque RLP byte strings | u64 big-endian byte length, then those exact bytes |
| uint256 magnitude | u64 big-endian byte length, then 0 to 32 minimal big-endian bytes; no leading zero; zero has length 0 |

The uint256 payload is the integer's magnitude, not its RLP prefix. RLP-valued columns retain
their canonical RLP bytes and use the binary rule. Missing nullable columns are materialized as
null at their schema position; unknown columns require a schema-version definition. The optional
`transactions.raw_envelope` is an exception: it always contributes `00` to content hashing,
whether stored or absent, because it is a redundant transport representation. A cleaner that
uses it MUST establish that it agrees with the structured transaction columns; its stored bytes
remain covered by the file hash. Other nullable columns are not silently normalized away.

For example, null encodes as `00`, present empty binary as `010000000000000000`, and present
binary `aabb` as `010000000000000002aabb`. Adjacent variable-width fields cannot alias. Golden
primitive and logs-row vectors are in `crates/legacy-format/src/canonical.rs` and `logs.rs`.
The implementation supports these primitives and all columns of `headers`, `transactions`,
`receipts`, `logs` and `withdrawals`. Transaction parity normalization and raw-envelope exclusion are
specified above; transaction envelope semantics remain unchecked. The trace codec, including
trace enum mappings/order, remains to be implemented and tested before its producers can
claim conformance.

### 11.2 Reorgs at the seal boundary

A relic is sealed **only once its entire range is past finality** (post-merge: beacon-finalized;
pre-merge: buried and covered by era1). The ExEx Shadow buffers unfinalized blocks in a short-lived
**tip index** (in-memory or small on-disk, not part of any relic) and promotes a range to a sealed
relic only when `finalized_block >= relic.end`. Reorgs below finality cannot occur; reorgs of the
tip are handled via the ExEx `ChainReverted`/`ChainReorged` variants before sealing.

### 11.3 Throughput

Backfill throughput is bounded by the source: Reth/Erigon/era1 Shadows are I/O- and
decompress-bound (local disk); the archive-RPC Shadow is bounded by upstream rate limits; the write
side is bounded by zstd-3 throughput. *(Design target, not a measurement: local-file Shadows should
sustain sealing well above chain growth so a mainnet backfill completes in days, not weeks; to be
benchmarked.)*

## 12. Chain-Specific Handling

- **Pre-Byzantium receipts:** 32-byte post-state root instead of status (§10.2).
- **EIP-2718/2930/1559/4844/7702:** typed envelopes; type in `transactions.type`; access lists
  (2930+), fee caps (1559+), blob fields (4844), authorization lists (7702) as dedicated/RLP columns
  (§6.4).
- **EIP-4895 withdrawals:** separate `withdrawals` table; `withdrawals_root` verified (§10.3).
- **EIP-4844 blob sidecars:** excluded; only `blob_versioned_hashes` retained (§10.4).
- **OP-stack:** deposit transactions are "a new EIP-2718 compatible transaction type with the prefix
  0x7E" with RLP fields `sourceHash, from, to, mint, value, gas, isSystemTx, data` and no signature
  (API responses zero the `v,r,s`); columns `source_hash`/`mint`/`is_system_tx`. Receipts carry
  `deposit_nonce` (set to the receipt's `depositNonce` from Regolith onward) and
  `deposit_receipt_version` (Canyon+), plus L1-fee fields. Deposit-nonce canonicalization uses the
  receipt-root-authenticated nonce from Canyon onward, zero before.
- **Arbitrum:** Arbitrum-specific transaction types and block fields are carried in `type` plus
  nullable Arbitrum-specific columns (§12.5). **The exact Arbitrum type-byte assignments and extra
  header fields must be verified against Nitro sources before the Arbitrum silo ships - flagged here
  as unverified.**
- **Polygon:** state-sync ("bor") events appear as synthetic logs/receipts (the end-of-block
  state-sync transaction). **The exact Bor state-sync encoding and system address must be verified
  against Bor sources before the Polygon silo ships - flagged here as unverified.**

### 12.5 Schema versioning and evolution

- New columns are added **nullable**; old relics simply lack them (absence = null). Readers MUST
  tolerate missing optional columns.
- New tables are added without touching existing tables.
- Existing relics are **never rewritten**; a `spec_version` bump applies only to newly sealed relics.
  A pact chain may therefore span multiple spec versions; each manifest records its own
  `spec_version`, and cleaning uses the version in the manifest.
- Breaking changes (renaming/removing a column, changing a type) are prohibited within a major spec
  version.

## 13. Serving (Solo)

### 13.1 Architecture

Async Rust on `tokio`. Recommended crates: `object_store` or `opendal` (S3/R2/GCS + local),
`parquet`/`arrow-rs` (readers, footer/bloom access), `roaring` (sidecars), `alloy` (Ethereum + RPC
types, trie helpers), `jsonrpsee` (JSON-RPC server) or `axum` (HTTP for MCP/x402), and
`reth-primitives` where reuse helps. Solo is a **splitting proxy**: it classifies each request as
(a) served-from-relics (finalized reads), (b) forwarded-to-upstream (tip/unfinalized, mempool), or
(c) rejected (historical state).

### 13.2 Finality boundary

Solo determines the finalized boundary from the upstream node via
`eth_getBlockByNumber("finalized")`, cached and refreshed on an interval, optionally with a
configured safety lag (extra confirmations). Requests at or below the boundary are eligible for
relic service; above it, forwarded.

### 13.3 JSON-RPC routing table

| Method | Action | Notes |
|---|---|---|
| `eth_getBlockByNumber` / `eth_getBlockByHash` | serve if <= finalized, else forward | assembles header + txs from relic |
| `eth_getBlockReceipts` | serve if finalized | from `receipts`+`logs` |
| `eth_getTransactionByHash` | serve if finalized (txhash sidecar), else forward | |
| `eth_getTransactionByBlockNumberAndIndex` / `…HashAndIndex` | serve if finalized | |
| `eth_getTransactionReceipt` | serve if finalized, else forward | |
| `eth_getTransactionCount` | forward | needs state |
| `eth_getLogs` | serve finalized range; split a range straddling the boundary | §13.5 |
| `eth_getBlockTransactionCountByNumber/Hash` | serve if finalized | |
| `eth_blockNumber` / `eth_chainId` / `eth_syncing` | forward | |
| `eth_call` / `eth_estimateGas` | forward (current) / **reject (historical block)** | see error below |
| `eth_getBalance` / `eth_getCode` / `eth_getStorageAt` | forward (latest) / **reject (historical block)** | |
| `debug_traceTransaction` / `trace_transaction` / `trace_block` | serve from traces tier if present, else reject | call traces only |
| `debug_traceCall` | **reject** | needs state execution |
| `eth_sendRawTransaction` / `eth_feeHistory` / `eth_gasPrice` / mempool | forward | |
| `net_*` / `web3_*` | forward | |

Rejection for out-of-scope historical-state calls uses JSON-RPC error `code: -32004`,
`message: "the-legacy: historical state is out of scope; route
eth_call/eth_getBalance/eth_getCode/eth_getStorageAt/debug_traceCall at historical blocks to an
archive node"`. Rejection is explicit and never a silent wrong answer.

### 13.4 Cache design

- **NVMe LRU** of relic files and/or individual row groups, sized by config. Parquet **footers** are
  pinned/cached separately (small, hot). Index **sidecars** for recent relics are pinned in RAM.
- Object-storage reads are HTTP byte-range: fetch footer, select row groups via min/max + sidecar,
  fetch only needed column chunks.

### 13.5 `eth_getLogs` execution plan (semantics parity)

1. **Range pruning:** map `fromBlock..toBlock` to the relic set (`>> 13`). If `blockHash` is
   present, `fromBlock`/`toBlock` are disallowed (EIP-234) and the query targets exactly one block.
2. **Bitmap sidecar:** for each `address` in the (possibly array) filter, load the address roaring
   bitmap; for each populated topic position, load the topic bitmap. Intersect address, topics and
   block-range to a candidate row-group set.
3. **Row-group bloom:** confirm candidate row groups with native column blooms before scanning.
4. **Scan:** read only candidate row groups' `address`, `topic0..3`, `data`, and index columns;
   apply the full filter.
5. **Assemble and order:** results ordered by (`block_number`, `transaction_index`, `log_index`),
   matching Geth/Reth; `removed` is always `false`. The address filter is an OR over addresses;
   topics are position-wise AND with per-position OR arrays and `null` wildcards (topic filter
   `[A,[B,C]]` matches topic0=A AND topic1 in {B,C}; topics are order-dependent per the spec).
6. **Caps:** enforce a configurable max results / max scanned blocks. Note there is **no** cap in
   the base JSON-RPC spec - provider caps vary widely (public gateways enforce anything from a few
   hundred to tens of thousands of blocks and 10k-log/10 MB response ceilings). The Legacy therefore
   adopts explicit, documented, configurable limits and, on exceed, returns a bounded error (e.g.
   `-32005`) so clients page.

### 13.6 Concurrency, metrics, footprint

- Concurrency limited by a global semaphore over object-storage requests and a per-request
  scanned-bytes budget.
- **Prometheus** metrics: per-method request counts/latency, relic cache hit ratio, bytes fetched
  from object storage, `eth_getLogs` rows scanned/returned, upstream-forward counts, a
  cleaning-status gauge.
- **Footprint targets (estimates):** Solo is designed for a small VM - **2 to 4 vCPU, 8 to 16 GB
  RAM**, with an NVMe cache of **100 to 500 GB** (hot relics + sidecars), serving a corpus of
  hundreds of GB to multi-TB that lives in object storage. RAM is dominated by pinned
  sidecars/footers and in-flight row groups, not the corpus. *(Design targets, to be validated.)*

### 13.7 Config example (TOML)

```toml
[server]
listen = "0.0.0.0:8545"
mcp_listen = "0.0.0.0:8546"

[chain]
chain_id = 1
silo = "Silo 1"

[storage]
backend = "r2"                 # r2 | s3 | gcs | local
bucket  = "legacy"
prefix  = "legacy/v1/1"
endpoint = "https://<accountid>.r2.cloudflarestorage.com"

[cache]
nvme_path = "/var/cache/solo"
nvme_max_gb = 300
pin_sidecars = true
pin_footers = true

[upstream]
url = "http://reth-pruned:8545"
finality_source = "finalized"  # eth_getBlockByNumber("finalized")
finality_lag_blocks = 0

[limits]
getlogs_max_blocks = 100000
getlogs_max_results = 100000
max_concurrent_object_reqs = 256
max_scanned_bytes_per_req = 2147483648

[verify]
checkpoint_list = "https://checkpoint-sync.example/eth/finalized"
clean_on_start = true

[x402]
enabled = false
price_per_relic_read_usdc = "0.001"
price_per_getlogs_scan_usdc = "0.005"
facilitator = "https://x402.example/facilitator"
```

### 13.8 Deployment

Solo registers as a finalized-history backend behind **erpc** or **proxyd**, which route finalized
reads to Solo and everything else to full nodes. This is the intended production topology.

## 14. AI-Native Surface

### 14.1 MCP server

Solo exposes an MCP server over JSON-RPC 2.0 (`tools/list` + `tools/call`, stdio or streamable
HTTP). Example tool definitions:

```json
[
  {"name": "legacy.describe_schema",
   "description": "Return the Parquet schema, spec version, and available tables/tiers for a chain.",
   "inputSchema": {"type":"object","properties":{"chain_id":{"type":"integer"}},"required":["chain_id"]}},

  {"name": "legacy.get_logs",
   "description": "Finalized eth_getLogs with identical semantics; paginated.",
   "inputSchema": {"type":"object","properties":{
     "chain_id":{"type":"integer"},"from_block":{"type":"integer"},"to_block":{"type":"integer"},
     "address":{"type":["string","array"]},"topics":{"type":"array"},"cursor":{"type":"string"}},
     "required":["chain_id","from_block","to_block"]}},

  {"name": "legacy.get_transaction",
   "description": "Fetch a finalized transaction + receipt by hash.",
   "inputSchema": {"type":"object","properties":{"chain_id":{"type":"integer"},"hash":{"type":"string"}},"required":["chain_id","hash"]}},

  {"name": "legacy.verify_receipt",
   "description": "Return a Merkle inclusion proof of a receipt/log against receiptsRoot plus the header-chain path to a checkpoint.",
   "inputSchema": {"type":"object","properties":{"chain_id":{"type":"integer"},"tx_hash":{"type":"string"}},"required":["chain_id","tx_hash"]}}
]
```

`legacy.verify_receipt` returns a structured proof: the MPT branch from the receipt leaf to
`receipts_root`, the header containing that root, and the `parent_hash` chain from that header to
the nearest trusted checkpoint - so a caller verifies inclusion without trusting Solo. Per the MCP
spec, tool results include both a `structuredContent` object and a serialized JSON text block for
backward compatibility, and clients treat tool annotations as untrusted unless from a trusted
server.

### 14.2 x402 pricing

Solo optionally gates paid endpoints with x402: on an unpaid request it returns HTTP
`402 Payment Required` with payment requirements (price in USDC, network, recipient, scheme); the
client retries with a signed payment payload header; Solo (or a facilitator's `/verify` and
`/settle` endpoints) validates before serving. Pricing units: per relic read and per `eth_getLogs`
range scan (scanned-bytes-based). This monetizes expensive scans and is a natural DoS throttle.

### 14.3 The Graph: Horizon / GraphTally integration

Graph Horizon (GIP-0066, mainnet December 2025) transforms The Graph into "a modular platform for any
type of blockchain data service," where a **data service** is "a service designed to run on Graph
Horizon that makes data available for consumers … Potential examples include: Subgraphs, Firehose,
Substream, SQL or LLM queries." Indexers **provision** stake to a specific data service ("creating a
data service 'provision'"). Payments use **GraphTally** (formerly TAP, the Timeline Aggregation
Protocol; GIP-0054): a gateway (the Sender) "attaches a cryptographically signed Receipt to each
request" - "a verifiable IOU" - and the indexer batches Receipts "into a single Receipt Aggregate
Voucher (RAV)" and "submits one RAV to the blockchain, compressing a large batch of micropayments
into a single transaction," redeemed via the escrow/TAPVerifier contracts. GRT is unchanged.

When The Legacy is served as a Graph data service, Solo is the **supply side**: it serves finalized
history and collects GraphTally receipts. Pete's own community proposals sit exactly here:
**GRC-005 "Dispatch"** (an experimental JSON-RPC data service on Horizon where "indexers stake GRT,
register to serve specific chains, and get paid per request via GraphTally (TAP v2) micropayments")
and **GRC-006 "Mainline"** (a Firehose data service on Horizon). **Honest caveat:** the framing of
GRC-005 as defining an "Archive capability tier" could **not** be verified against official Graph
sources - no officially named "Archive" data-service tier exists in Horizon today, and GRC-005 is
the "Dispatch" JSON-RPC data service RFC (both GRC-005 and GRC-006 are community RFCs, not ratified
protocol services). The Legacy is positioned as the supply side of such an archive/JSON-RPC data
service, not as an already-ratified tier.

### 14.4 Direct SQL surface

Because relics are plain Parquet with native blooms and min/max stats, DuckDB/DataFusion/Polars read
them directly over HTTP byte-range; the registry supplies the URLs and the pact root proves
integrity. Sample DuckDB query (all USDC `Transfer` logs in a range):

```sql
INSTALL httpfs; LOAD httpfs;
SELECT block_number, transaction_hash, topic1 AS from_addr, topic2 AS to_addr, data
FROM read_parquet([
  'https://legacy.example/legacy/v1/1/relics/002451/logs.parquet',
  'https://legacy.example/legacy/v1/1/relics/002452/logs.parquet'
])
WHERE address = '\xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48'::BLOB
  AND topic0 = '\xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef'::BLOB
  AND block_number BETWEEN 20078592 AND 20086783
ORDER BY block_number, log_index;
```

## 15. Performance Targets

All figures are **design targets, not measurements**, with reasoning stated:

- **tx-hash lookup:** warm (sidecar + footer cached) **< 20 ms**; cold (fetch sidecar + one row
  group from object storage) **< 200 ms**. Reasoning: one SBBF check + one binary search + one
  byte-ranged column-chunk read; cold latency is dominated by a single object-storage round-trip.
- **`eth_getLogs`, single address, 1M blocks:** warm **< 1 s**; cold **a few seconds**. Reasoning:
  roaring intersection selects a small candidate row-group set; only those chunks are fetched.
  Targets parity-or-better vs Geth/Reth for wide single-address queries, whose cost scales with
  blocks scanned.
- **Full-range backfill / read throughput:** bounded by object-storage bandwidth, not CPU - a mirror
  saturates its network link.
- **Comparison and falsification.** Envio HyperSync markets itself as "up to 2000x faster than RPC
  for getting or fetching the logs, transactions, traces, and blocks across 86+ chains"; SQD's query
  engine reports per-RPC-call speedups over its own legacy engine. The Legacy targets the same
  **order of magnitude** for finalized reads while remaining an open, verifiable, self-hostable
  corpus rather than a hosted service. **The benchmark that would invalidate the design:** if, on a
  warm NVMe cache, a single-address 1M-block `eth_getLogs` is not materially faster than a
  well-tuned Reth archive node answering the same query - i.e. if roaring+bloom pruning fails to
  beat linear receipt/log scanning - the sidecar architecture's core premise is wrong and should be
  revisited.

## 16. Security and Threat Model

- **Malicious mirror serving fabricated relics:** defeated by **cleaning** - any client rebuilds
  tries and checks header roots + checkpoint anchor; fabricated data fails.
- **Malicious producer:** optional manifest signatures (ethereum-secp256k1 or ed25519) plus
  **cross-producer content comparison** for matching tables and ranges. A matching content hash
  establishes agreement, not correctness or independent provenance. Pact comparison and O(log n)
  divergence localization apply to mirrors of the same manifest chain, not differently framed
  independent productions. Cleaning supplies the chain-commitment checks.
- **Poisoned traces:** **explicitly not defended by cryptography.** Traces are not header-committed;
  a malicious producer can fabricate them and no root will catch it. Trust in traces rests solely on
  cross-producer agreement or local re-execution. Stated plainly.
- **DoS via expensive `eth_getLogs`:** result/scan caps, global concurrency and scanned-bytes
  budgets, and optional x402 pricing on scans.
- **Object-storage supply chain:** every file is content-addressed by BLAKE3 in a hash-chained
  (optionally signed) manifest; a tampered object fails its hash.
- **What Solo trusts:** (1) its upstream node for the unfinalized tip and finality boundary; (2) the
  configured checkpoint list for the post-merge header-chain anchor. Everything below finality is
  self-verifiable.

## 17. Prior Art (brief)

- **era1 / EIP-4444 / Portal:** complementary; used as a verifiable **input** (era1 provides
  headers/bodies/receipts + SSZ accumulator).
- **Reth static files (NippyJar):** a Shadow source; client-internal, not a serving/verification
  spec.
- **Erigon snapshots (`.seg`):** a Shadow source; content-addressed, BitTorrent-distributed - the
  distribution precedent The Legacy follows.
- **cryo:** the closest Parquet precedent (and sizing reference), but no
  manifest/pact/verification/serving.
- **Firehose:** the traces Shadow source.
- **HyperSync (closed) / SQD (Parquet lake, token-gated):** performance references; The Legacy
  differs by being open and verifiable.
- **erpc / proxyd / dshackle:** deployment partners; Solo sits behind them as the finalized backend.
- **Nuthatch (Pete's indexer):** its cold store is already sealed Parquet segments; it will consume
  The Legacy as its raw layer.

## 18. Roadmap

- **Stage 1 - Foundation.** Spec (this RFC) + Solo (finalized block/tx/receipt/log reads) + mainnet
  headers/txs/receipts/logs relics on R2 + Reth static-file Shadow. *Acceptance:* Solo answers
  `eth_getLogs`/`eth_getTransactionByHash`/`eth_getTransactionReceipt`/block reads for finalized
  mainnet with Geth-parity results; a fresh mirror bootstraps from the registry and passes cleaning
  end-to-end; canonical table content hashes match across two independent producers, and exact
  mirrors agree on pact roots.
- **Stage 2 - Continuous + verifiable + monetizable.** Reth ExEx Shadow (continuous sealing) +
  Dispatch/Horizon (GraphTally) integration + MCP + x402 + `verify_receipt` proofs. *Acceptance:*
  ExEx seals new relics automatically at finality with no manual step; MCP `verify_receipt` returns
  a proof a third party validates independently; x402-gated scans settle via a facilitator; Solo
  collects a GraphTally receipt on a Horizon testnet.
- **Stage 3 - Traces + L2s.** Firehose traces Shadow + L2 silos (Arbitrum, Optimism, Base).
  *Acceptance:* the traces tier passes cross-producer content-hash agreement between Firehose and
  Reth-re-execution Shadows; at least one L2 silo ships with verified transactions/receipts/logs
  relics and a published pact root.

## 19. Open Questions and Alternatives Considered

- **8192 vs 10,000 blocks:** chose **8192** (era1 alignment, shift-based addressing). 10,000
  rejected (desyncs era1, forces accumulator recompute). §5.
- **Logs nested vs flat table:** chose **flat** (blooms/bitmaps on topic columns, column
  projection). Nested rejected (defeats per-column pruning). §6.
- **Parquet native blooms vs sidecars:** chose **both** (native for SQL portability, roaring sidecars
  for Solo latency). §7.4.
- **BLAKE3 vs SHA-256:** chose **BLAKE3** (bulk-verify throughput). SHA-256 rejected despite
  ubiquity. §8.2.
- **TOML vs JSON vs CBOR manifest:** chose **canonical JSON (JCS)** (canonicalizable + auditable).
  TOML rejected (no canonical form); CBOR rejected (not human-auditable). §8.1.
- **Arrow IPC / Lance instead of Parquet:** rejected - Parquet is the interop standard read by
  DuckDB/DataFusion/Polars/Spark; Lance/Arrow-IPC would sacrifice the ubiquitous SQL surface for
  marginal gains.
- **Custom binary protocol vs JSON-RPC:** JSON-RPC only, so Solo is a drop-in behind erpc/proxyd. A
  binary protocol is a possible future optimization, not v1.
- **Storing traces at all:** kept as an **optional tier** with the verification gap stated; some
  consumers need call traces (e.g. native-ETH-transfer tracking) and there is no header-committed
  alternative.
- **Rust-native query engine vs embedded DuckDB in Solo:** Solo uses a **purpose-built reader**
  (roaring + arrow-rs) for its hot paths; the ad-hoc SQL surface is left to external
  DuckDB/DataFusion against the plain Parquet. Embedding DuckDB in Solo was rejected to keep the
  binary small and the hot path predictable. **Open question:** whether to embed DataFusion for
  ad-hoc SQL served by Solo directly.
- **Open - determinism:** whether pinned writer settings + a fixed arrow-rs version can achieve
  routine byte-identity, or whether content-identity is the permanent conformance bar (§11.1).

## 20. References

- Apache Parquet - Bloom Filter (Split Block Bloom Filter) spec; `parquet-format/BloomFilter.md`;
  Jim Apple, "Split block Bloom filters," arXiv:2101.01719.
- RoaringBitmap - RoaringFormatSpec (portable serialization); `roaring` (Rust), CRoaring.
- eth-clients/e2store-format-specs - `era1.md`, `era.md`; status-im/nimbus-eth2 `e2store.md`;
  go-ethereum `internal/era`.
- Reth - static files / NippyJar; `reth_exex` (`ExExNotification`, `ExExContext`,
  `ExExEvent::FinishedHeight`); `reth_era`.
- Erigon - snapshots / `.seg` and downloader docs; `erigon-seg` (Rust) for `.kv/.bt/.kvei`.
- StreamingFast Firehose - `pbbstream` / merged-blocks; firehose-core.
- EIP-2718 (typed tx envelope), EIP-2930, EIP-1559, EIP-4844 (blob tx), EIP-7702, EIP-4895
  (withdrawals), EIP-658 (status receipts), EIP-4444 (history expiry), EIP-234 (blockHash filter).
- OP Stack specs - deposits (type 0x7E), deposit receipts (Regolith/Canyon); op-geth.
- Ethereum JSON-RPC spec - `eth_getLogs`, block/tx/receipt methods.
- Model Context Protocol - specification (tools; JSON-RPC 2.0).
- Coinbase x402 - protocol repo and docs.
- The Graph - GIP-0066 (Horizon), GIP-0054 (TAP/GraphTally), GraphTally docs; GRC-005 (Dispatch),
  GRC-006 (Mainline).
- banteg.xyz ("Ethereum Transfers Heatmap") and degencode.com - cryo extraction sizes; Cloudflare R2
  pricing (developers.cloudflare.com/r2/pricing) and AWS S3 pricing.
- Envio HyperSync docs; SQD query engine - performance references.

---

*Prepared by Pete (Petko Pavlovski) for publication as RFC-0001 in `nuthatch-org/the-legacy`. Every
format and protocol fact above is drawn from the primary sources listed in §20; every quantitative
projection is labeled an estimate or a design target; and the two verification gaps that matter -
traces are not header-committed, and L2 corpora are much larger than mainnet - are stated without
softening. Two chain-specific areas (Arbitrum transaction types/header fields, Polygon Bor
state-sync encoding) are explicitly flagged as unverified and must be confirmed against client
sources before those silos ship.*
