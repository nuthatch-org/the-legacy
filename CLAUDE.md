# the-legacy

A sealed, verifiable history corpus for EVM chains, plus the binary that serves it.
**[RFC-0001](docs/rfcs/0001-the-legacy.md) is the specification. This repository implements it.**
[RFC-0002](docs/rfcs/0002-the-backfill-layer.md) proposes the backfill follow-up; its §8 lists
implementation-linked amendments, not already supported interfaces.

## Ground rules

1. **The RFC is normative.** If the code and RFC-0001 disagree, one of them is a bug; say which
   before changing either. A spec change and the code change that follows it belong in the same PR.
2. **Never overstate verification.** Every report, log line, doc and README row must say what was
   actually checked and what was not. `solo clean` currently checks manifest structure, relic
   linkage and the pact chain by default, and reports a run anchored at a declared
   `detached_pact_start` (RFC-0001 §8.4.1) as having no established link to genesis. `--files` adds local byte integrity, Parquet counts and
   decoded checks for all five core tables, including header coverage and stored links. It
   reconstructs Ethereum header hashes through Prague for chain ID 1 only and checks transaction,
   receipt and log references plus receipt/header blooms where the required tables exist. Chain
   ID 1 also checks receipt/header execution gas accounting plus transaction and receipt trie
   roots. When a manifest carries an era1 accumulator root and headers were read, that
   root is recomputed from the stored block hashes and total difficulties. A missing root
   stays unchecked. The comparison does not reconstruct header RLP and does not decide
   whether the range is pre-merge. Fees, other derived fields, other chain profiles, completeness,
   execution/fork validity and checkpoint trust remain
   unchecked; consistency is not chain trust.
   `solo serve` admits a local corpus only after those file checks and repeats their result in
   `legacy_capabilities`. It does not add trie, anchor or index checks of its own.
   This is the single most important rule in the repo: the whole project's
   value is that its claims are true.
3. **Traces are not header-committed.** No cryptographic claim about the traces tier is acceptable
   anywhere - code, comments, docs, marketing. Cross-producer agreement or local re-execution, and
   that is the lot (§6.8, §16).
4. **Relics are immutable.** Nothing rewrites a sealed relic. Schema evolution is additive and
   nullable; a `spec_version` bump applies only to newly sealed relics (§12.5).
5. **Seal only past finality.** A relic covers 8192 blocks and is sealed once its entire range is
   final. Tip handling lives outside relics (§11.2).

## Shape

- `crates/legacy-format` - geometry, canonical JSON (JCS), manifests, the pact chain, the registry.
  Canonical row primitives, headers/transactions/receipts/logs/withdrawals rows and their content hashes
  live here. Ethereum transaction and receipt trie root reconstruction exists for raw legacy and
  types 1..=4 envelopes, plus EIP-4895 withdrawal roots; no other chain profiles exist.
  No Parquet, no object storage, no JSON-RPC. Shadow and Solo agree on what a relic is by depending
  on this, not by both being careful.
- `crates/legacy-parquet` - Parquet schemas, shared writer profile and codecs for the implemented
  tables, plus verification against manifest file claims. No sealing, consensus validation or
  checkpoint verification.
- `crates/legacy-reader` - native local sealed-history reads. It currently re-verifies
  manifest-bound files before yielding core-table rows; object storage and indexed queries remain
  to be built.
- `crates/shadow` - the six ingestion sources (§11). era1 seals one aligned 8192-block file
  into headers, transactions, receipts and logs. The archive-RPC source (RFC-0004 §7) journals
  validated `eth_getBlockByNumber` + `eth_getBlockReceipts` responses under a request budget,
  resumes from the journal, rebuilds envelopes for types 0..=4 and seals all five tables. It is
  tested against local fixtures only; no real endpoint has been run. The other four are skeleton.
- `crates/solo` - serving (§13) and cleaning (§10). `solo serve` is sealed-only JSON-RPC over a
  local corpus: no upstream and no object storage. Log address and topic bitmaps are rebuilt in
  memory from the admitted files, as are transaction-hash and block-hash indexes. None of
  those indexes are sealed into the manifest. Checkpoint anchoring does not exist.

## Working

- Rust, no unsafe, comments explain *why* rather than restating the code.
- `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo deny check`
  all green before a commit. `yatr ci` runs the four.
- The MSRV in `Cargo.toml` is the toolchain that has actually been built and tested on. Lowering it
  needs a green run on the lower toolchain, not optimism.
- No em dashes in prose. Hyphens, or restructure.
- Branches `pete/<short-description>`. Commit messages single-line, no body, no AI attribution.
