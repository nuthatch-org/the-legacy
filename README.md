# the-legacy

> **Everything from before, sealed, kept by one small process.**
> An open, verifiable, mirrorable history corpus for EVM chains, and a thin Rust binary that serves
> it.

[![ci](https://github.com/nuthatch-org/the-legacy/actions/workflows/ci.yml/badge.svg)](https://github.com/nuthatch-org/the-legacy/actions/workflows/ci.yml)

EIP-4444 makes historical execution data a second-class citizen of the base protocol at exactly the
moment demand for it is climbing. The existing answers are each half of one: era1 and Portal are
verifiable but not query-friendly; Reth static files and Erigon snapshots are client-internal
formats rather than specs; cryo writes excellent Parquet with no manifest, verification or serving
story; HyperSync is closed and SQD is token-gated.

The Legacy is the spec for the missing half, plus the code to produce and serve it. Finalized
history - headers, transactions, receipts, logs, withdrawals, optionally call traces - is sealed
into immutable 8192-block segments called **relics**: plain Apache Parquet plus a canonical
manifest. Manifests chain into a **pact**, one root hash per chain per height, so two mirrors
compare their entire corpus in a single 32-byte exchange and localise any disagreement in O(log n)
requests. Nothing in it privileges the producer: every file is content-addressed and every
sidecar is rebuildable. The current cleaner rebuilds transaction and receipt tries for its chain
ID 1 profile.

**Read [RFC-0001](docs/rfcs/0001-the-legacy.md) first.** It is the specification; this repository is
its implementation. [RFC-0002](docs/rfcs/0002-the-backfill-layer.md) is the follow-up draft for
node-free backfill, native readers and cost-bounded production; its interfaces remain proposed.

## Status

Early. The spec is written, the format layer is implemented and tested, and **nothing serves
anything yet.** Precisely:

| | state |
|---|---|
| RFC-0001 | written, Draft |
| Relic geometry, canonical JSON (JCS), manifests, pact chain, registry | implemented |
| Canonical row primitives and headers/transactions/receipts/logs/withdrawals content hashing | implemented, golden byte vectors |
| `solo clean` (manifest structure, relic linkage, pact chain) | implemented; `--files` adds local integrity checks |
| Headers, transactions, receipts, logs and withdrawals Parquet codecs | implemented, local synthetic round trips; no sealer |
| Log address/topic bitmaps | rebuilt in memory for `eth_getLogs`; not manifest-committed |
| Tx and block hash indexes | in-memory sorted records plus a split-block bloom; not manifest-committed |
| Traces Parquet codec | not started |
| Ethereum receipt trie-leaf encoding | implemented for legacy and types 1..=4; independent synthetic vectors |
| Ethereum transaction trie roots | chain ID 1 local cleaner check from stored raw envelopes |
| Ethereum receipts trie roots | chain ID 1 local cleaner check; header trust remains separate |
| Ethereum withdrawal trie roots | chain ID 1 local cleaner check for EIP-4895 rows |
| Checkpoint anchoring | not started |
| `legacy-reader` | local, manifest-verified sealed core-table scans; object storage and indexed queries not started |
| era1 Shadow | seals one aligned 8192-block era1 file; partial and unaligned files are refused |
| era1 accumulator | recomputed when the manifest carries `era1_accumulator_root` and headers are read |
| `solo serve` | sealed-only JSON-RPC on a local corpus; no upstream; log bitmaps are process-local |
| the other five Shadow sources | not started |

`solo clean` says out loud which checks it performed and which it did not, and will keep doing so
until each one is real. A verification report that implies more than it checked is worse than no
report at all.

The workspace has 149 tests. `solo clean --files` checks each listed file's size, BLAKE3 and
Parquet footer counts. For all five core tables it also checks schema, row order, canonical
content hash, decoded count and block bounds. Headers must cover every block in the
relic, have consistent stored parent links, and match the manifest boundaries. For chain ID 1,
it also reconstructs RLP/Keccak header hashes using Ethereum layouts through Prague; other chain
profiles remain unchecked. Hash-consistent synthetic chains can still pass. Consensus rules
(including fork activation), other table contents, finality and checkpoint trust remain explicitly
unchecked. Transaction field agreement, signatures and sender recovery
also remain unchecked, as do fees and other derived fields.
When the relevant tables are present, the cleaner matches transaction/receipt keys, hashes and
types, each log's transaction and receipt references, and receipt blooms rebuilt from log
addresses/topics. It also compares header blooms with the OR of supplied receipt blooms per
block, independently of whether logs are available. Blooms do not cover log data or prove
completeness. Per-relic reports distinguish absent
tables from empty ones. Agreement among supplied rows does not establish completeness or trie
correctness; the checker retains these decoded tables for one relic in memory. For chain ID 1,
it also checks contiguous receipt indices, cumulative execution gas, present `gas_used` values,
header gas totals and the block gas limit. Other chains report this accounting profile as unchecked.
For chain ID 1 with headers and transactions present, it rebuilds the transaction trie from raw
legacy or types 1 through 4 envelopes, checks each envelope's Keccak hash, and compares the root
with the header. With headers, receipts and logs present, it also rebuilds the receipt trie. These
checks prove agreement among supplied bytes, not canonical-chain membership, field agreement or
table completeness.
With headers and withdrawals present, it rebuilds EIP-4895 withdrawal tries, using a withdrawal's
position in the block as the trie key and its global index as part of the RLP value.
When a manifest carries `era1_accumulator_root` and headers were read, it recomputes the era1
SSZ accumulator from those headers' block hashes and total difficulties. A missing root stays
unchecked. That comparison does not reconstruct header RLP and does not decide whether the
range is pre-merge.

## Try it

In-memory table round trips using synthetic data, with no RPC or object-storage calls:

```sh
cargo run -p legacy-parquet --example logs_round_trip
cargo run -p legacy-parquet --example withdrawals_round_trip
```

The example prints separate file and canonical content hashes. File bytes depend on Parquet
framing; content hashes compare rows for the same chain, schema, range and table. Pact roots
compare exact manifest chains, not independently framed productions.

For an existing local corpus, keep each table file beside its manifest and use:

```sh
cargo run -p solo -- clean --files --json path/to/000000/manifest.json
```

Pass manifests in ascending relic order. A run starting after genesis also needs
`--after path/to/predecessor/manifest.json`; that predecessor supplies pact context and its table
files are outside the reported check scope. Without `--files`, cleaning remains manifest-only.
Missing, altered or malformed files fail the run with a nonzero exit and no success report.
Only regular local files with their canonical table names are read; table symlinks are refused.
The initial implementation holds one whole file and its decoded rows in memory, so it is not yet
a bounded-memory corpus scanner.

```sh
cargo run -p solo -- relic 20086783
```

```
block       20086783
relic       2451  (002451)
blocks      20078592..=20086783
prefix      legacy/v1/1/relics/002451
```

```sh
cargo run -p shadow -- seal --source era1 --file path/to/00000.era1 --from 0 --to 8191 --out path/to/relic
```

The file must be one complete 8192-block epoch aligned to a relic boundary. Anything shorter,
including the partial era that stops at the merge, is refused and nothing is written. A relic
after genesis also needs `--after path/to/predecessor/manifest.json`. The command reads only
that file. It does not contact a network and it does not publish the relic.

Serve that directory. The process admits the corpus only after the same file checks as
`solo clean --files`, then answers sealed history. There is no upstream. `latest`, `safe` and
`finalized` are the sealed head. `eth_getLogs` uses in-memory address and topic bitmaps rebuilt from the logs file. Those bitmaps
are not in the manifest. A topic hit still decodes the matching row groups, and the rows are
checked again. A transaction or block hash is resolved through an in-memory sorted array and
a split-block bloom, then that relic's row is read and the hash is checked. The indexes are
not in the manifest. A range above that
head, or past the configured log limits, is an error, not a shorter answer.

```toml
bind = "127.0.0.1:8545"
manifests = ["path/to/relic/manifest.json"]

[limits]
getlogs_max_blocks = 100000
getlogs_max_results = 100000
batch_max = 1000
```

```sh
cargo run -p solo -- serve --config solo.toml
```

```sh
cargo run -p shadow -- plan --from 20078592 --to 20090000
```

```
blocks 20078592..=20090000 cover 2 relic(s), 2451..=2452
  002451  blocks 20078592..=20086783  legacy/v1/1/relics/002451
  002452  blocks 20086784..=20094975  legacy/v1/1/relics/002452  (partial: the relic is sealed only once its whole range is past finality)
```

## Layout

```
crates/legacy-format   the executable half of the spec: geometry, JCS, manifests, pact, registry
crates/legacy-parquet  headers/transactions/receipts/logs/withdrawals codecs, writer profile and file verification
crates/legacy-reader   native local reads that re-verify manifest-bound bytes before yielding core rows
crates/shadow          the ingesters that transcode chain history into relics
crates/solo            the serving binary, and the cleaning that verifies what you are served
docs/rfcs/             RFC-0001 and successors
```

`legacy-format` includes canonical row encoding and deliberately knows nothing about Parquet,
object storage or JSON-RPC. Shadow and
Solo agree on what a relic *is* by depending on it, rather than by both being careful.

## Working on it

```sh
yatr ci     # fmt-check, lint, test, deny - the same four everywhere in this org
yatr test
```

or the long way round with `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`,
`cargo test`, `cargo deny check`.

## Licence

MIT or Apache-2.0, at your option.
