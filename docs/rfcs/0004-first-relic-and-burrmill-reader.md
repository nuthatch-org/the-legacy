# RFC-0004: First Sealed Relic and the Burrmill Reader

5 Nov 2026 · Pete

> **Amendments since the draft, 2026-11-05.**
> 1. §8.2's zero-predecessor start was not allowed by RFC-0001 §8.4, and neither was sealing 2078
>    first, which needs 2077 and so on back to genesis. RFC-0001 §8.4.1 now defines a declared
>    `detached_pact_start`, and §7.5, §8.2 and §10.3 are to be read through it.
> 2. §12.1's expectation was off by one. The Shapella activation block 17,034,870 carries a
>    `withdrawalsRoot` and no withdrawals. The first withdrawal, global index 0, is in block
>    17,034,871, as served by GraphOps and journaled on 2026-11-05.
> 3. The org is `nuthatch-org`, not `nightswatchhq`.

## 1. Status and placement

Draft, 2026-11-05, author Pete. Home: `docs/rfcs/0004-first-relic-and-burrmill-reader.md` in
nuthatch-org/the-legacy. RFC-0003 stays reserved for Porter (RFC-0002 P6), so this takes 0004. The
Burrmill half (§11) is filed in nuthatch-org/burrmill as a short companion, RFC-0048, that points
here rather than restating the format. Burrmill's RFC-0044 §6 already cites a different "RFC-0004"
(the nuthatch measurement RFC), so the companion must reference this one by repo and title, never by
number alone.

Relation to what exists:

- RFC-0001 is normative for the relic format, the manifest, the pact and cleaning. Nothing here
  changes it. Where this RFC reads as needing a format change, that reading is a bug in this RFC.
- RFC-0002 P4 (archive-RPC Shadow as a cost-bounded producer) is the design this RFC executes for
  one relic. P4's fixtures-first acceptance and budget rules are adopted whole; §7 only adds what a
  free, rate-limited endpoint changes.
- RFC-0002 P2 (legacy-reader) is the crate Burrmill consumes. This RFC does not extend the reader's
  query surface; Burrmill is the query surface.
- Burrmill RFC-0044 §3.6 (positive provider allowlist, no file functions) is the constraint the
  relics provider is built under. RFC-0047 (private analysis mode) is the consumer story the
  showcase feeds into.

What the repos already have, as of the clones read on 2026-11-05: the era1 Shadow seals one aligned
8192-block file; `solo clean --files` does the chain ID 1 checks listed in CLAUDE.md;
legacy-reader does manifest-verified local scans of the five core tables; `solo serve` serves a
local corpus sealed-only. The archive-RPC Shadow is an enum variant with no implementation. Burrmill
is DataFusion 55 hosted, opens nuthatch nests through `Engine::open_nest` and object stores through
`open_store`, and registers providers only through its own catalogue.

## 2. Summary

Seal one post-merge mainnet relic from the archive-RPC Shadow against the GraphOps endpoint, clean
it with the chain ID 1 checks that exist, publish it on object storage, and query it with Burrmill
through a relics provider that reads the manifest and the five core Parquet tables. The result is
the first relic in Silo 1 that was not produced from an era1 file, the first corpus read by Burrmill
that is not a nuthatch nest, and one reproducible showcase: fetch the relic, check its hash, run one
SQL statement, get a number you can check against a block explorer.

Three deliverables, in dependency order:

1. `shadow seal --source rpc`: a journaled, resumable, budgeted ingester that turns
   `eth_getBlockByNumber` and `eth_getBlockReceipts` responses into the canonical rows RFC-0001 §6
   defines and seals them with the existing format and Parquet crates.
2. A published relic directory on object storage under the RFC-0001 §9 path convention, with
   `registry.json`, and a `solo clean --files` report attached verbatim.
3. `burrmill::Engine::open_relics`: a catalogue entry point that admits a relic directory or a
   mirror URL, verifies what it can, and exposes headers, transactions, receipts, logs, withdrawals
   and a `relics` metadata table under Burrmill's allowlist, with `_dec` and `_overflow` beside every
   uint256 column.

The pre-merge era1 Shadow is used once more, not as a deliverable but as the oracle: the same
pre-merge relic sealed from era1 and from RPC must produce equal per-table content hashes (RFC-0001
§11.1). That is the cross-producer agreement RFC-0002 P4 asks for, and it costs nothing beyond a
second run.

## 3. Motivation

The Legacy has a format, a cleaner and a reader, and no relic anyone can download. Burrmill has an
engine that beat DuckDB on nuthatch nests and has never read anything else. Neither claim is
testable by a stranger until a relic exists on a public bucket. One relic turns both projects from
specs into things.

Why the RPC source and not era1 again: era1 covers pre-merge only, and the users below want
post-merge history. A free, rate-limited provider makes the archive-RPC Shadow the cheapest route to
a post-merge relic, and the P4 design (journal, budget, resume) is exercised for real instead of
against fixtures only. The GraphOps endpoint being free changes the cost model, not the design:
budgets and probes stay, because the next silo will not be free.

| User | What they do today | What one published relic proves |
|---|---|---|
| Indexer operators backfilling | Pay archive-RPC rates or wait for a node sync | A relic downloads and reads locally through legacy-reader, no node and no RPC |
| Analysts who want reproducible queries | Trust a hosted platform, or run cryo plus DuckDB | One SQL statement over a header-verifiable file, exact integers, no C++ |
| Auditors and incident responders | Re-derive numbers from an RPC they cannot cite | A manifest hash plus a cleaning report, re-runnable by anyone |
| Researchers | Cite "queried an RPC in March" | Cite a relic index and a manifest hash |
| Nuthatch | Cold store sealed by nuthatch itself | Relics as its raw backfill layer, read by the engine it already ships |

What the showcase proves for each project is different and both should be said plainly. For The
Legacy: a sealed segment is useful without Solo running. For Burrmill: the owned-execution,
positive-allowlist engine works on a corpus it did not seal, and its refuse-on-overflow story has a
natural EVM demonstration (§12).

The realistic first users are Pete via nuthatch and a handful of Graph indexers who would try a
cheaper backfill. Two of them using it is the signal; the post is the invitation.

## 4. Goals and non-goals

### 4.1 Goals

- One post-merge Silo 1 relic sealed by `shadow seal --source rpc`, passing
  `solo clean --files --json` with every chain ID 1 check the cleaner performs today, published on
  object storage with `registry.json`.
- One pre-merge Silo 1 relic sealed twice, from era1 and from RPC, with equal `content_hash` for
  every table the two sources both produce.
- A journaled, resumable RPC ingester whose budget accounting matches RFC-0002 P4: reserve before
  dispatch, count probes and retries, stop before an unfittable relic, never overspend to reach a
  boundary.
- `burrmill::Engine::open_relics` over a local directory and over an object store URL, exposing
  the five core tables and a `relics` metadata table, with exact uint256 handling (§11.4).
- One showcase statement whose result a reader can cross-check against an independent source, and
  whose refusal behaviour on uint256 overflow is demonstrated rather than described.
- A written post that states exactly what was verified, in the cleaner's own words, and nothing
  more.

### 4.2 Non-goals

- The unsealed tip, finality discovery beyond a one-shot check at seal time, and anything Porter
  (RFC-0003) will own.
- Traces. The RPC source does not produce them and no claim about them appears anywhere in the
  showcase.
- Checkpoint anchoring and header-chain trust beyond what `solo clean` does today. The relic is
  consistent with itself and with the headers it stores; canonical-chain membership is not
  established by this work, and the post says so.
- Index sidecars (RFC-0001 §7) and the tx address sidecars of RFC-0002 P3. Burrmill reads Parquet
  with its own pruning; sidecars come later.
- A general multi-relic corpus. Two relics (one pre-merge, one post-merge) are enough; the registry
  format already handles more.
- Serving through Solo. Solo may be pointed at the published relic as a smoke test, but it is not
  on the critical path.
- Changing Burrmill's admitted subset. The showcase uses shapes Burrmill already admits; a relic
  column that needs a new shape is reported as a gap, not patched in.

### 4.3 What verification covers, stated once

The cleaner checks, for chain ID 1: file sizes and BLAKE3, Parquet footer counts, schema and row
order, canonical content hashes, header coverage and stored parent links against the manifest
boundary, reconstructed header hashes, transaction and receipt trie roots from stored envelopes and
receipts, receipt and header blooms, execution gas accounting, and withdrawal roots where the table
exists. It does not check consensus rules, fork activation, sender recovery, fee derivation,
completeness against the canonical chain, or any trust anchor. Every document this RFC produces
repeats that boundary where a reader could otherwise infer more.

## 5. Terminology and conventions

RFC-0001 §4 terms are used unchanged: relic, pact, cleaning, Solo, Shadow, silo. RFC-0002 §3 adds
sealed head, reader and active blocks. This RFC adds:

- **Journal:** the on-disk record of validated RPC responses for one relic in progress, keyed by
  block number, from which sealing resumes after a crash. Mirror-local, never published, never part
  of a relic.
- **Budget:** the pair `--budget-requests` and `--budget-cu` from P4. Against the GraphOps endpoint
  the CU budget is informational and the request budget is the real limit.
- **Provider profile:** an offline TOML description of what an endpoint supports and how it limits
  (methods, batch size, requests per second, history depth). Discovery probes fill a profile; a
  profile can also be hand-written so `shadow plan` works with no network.
- **Oracle relic:** a pre-merge relic sealed from era1, used only to compare content hashes with
  the RPC production of the same range.
- **Showcase relic:** the post-merge relic that is published and queried.
- **Relics provider:** Burrmill's catalogue entry for a relic corpus, one TableProvider per core
  table plus the `relics` metadata table.

Conventions carried from the repos: no em dashes in prose; never overstate verification; branches
`pete/<short-description>`; single-line commits. Numbers in this RFC that are estimates are labelled
as estimates. Block numbers named in §6 are to be confirmed against the chain before any seal is
run; they are not authoritative here.

## 6. Segment selection

Two relics, chosen by what they let the cleaner and the showcase prove, not by convenience.
`relic_index = block >> 13`; ranges are `[index * 8192, index * 8192 + 8191]`.

### 6.1 Showcase relic: 2079, the Shapella relic

Shanghai/Capella activated at mainnet block 17,034,870 (2023-04-12). That block lies in relic 2079,
range 17,031,168 to 17,039,359. It is the strongest candidate because:

- The withdrawals table starts partway through the relic: rows absent before activation, present
  after. That exercises the one table the era1 source never produces and the EIP-4895 withdrawal
  root check the cleaner already has.
- Every transaction type through 2 is present at volume, so the trie reconstruction runs over real
  London-era envelopes, not synthetic ones.
- The headline numbers are easy for a reader to cross-check: the first withdrawal ever, total gwei
  withdrawn in the relic, and ERC-20 flow of a well-known token in the same window.
- Being post-merge, difficulty is 0 and total_difficulty is null, which is the path era1 never
  tested.

To confirm before sealing: the activation block number against the Ethereum fork registry, and that
the GraphOps endpoint serves `eth_getBlockReceipts` for that depth (§7.2).

Alternatives considered: relic 2371 (Dencun, 19,426,587, first type 3 blob transactions; attractive
but adds `max_fee_per_blob_gas` and `blob_versioned_hashes` edge cases to a first run) and relic
1896 (the merge boundary, 15,537,394; rejected because RFC-0001 §5 says that relic needs explicit
handling of a partial era1 accumulator and it should not be the first thing the RPC Shadow meets).
Either can be the second showcase.

### 6.2 Oracle relic: a full pre-merge era1 epoch

Any aligned pre-merge relic for which the era1 file is already on disk from the era1 Shadow's own
testing. Proposed: relic 209, range 1,712,128 to 1,720,319 (June 2016, inside the DAO episode;
dense enough to be a real test, legacy-only transactions so the envelope reconstruction path is the
simplest one). The era1 production is the oracle; the RPC production must match it on headers,
transactions, receipts and logs content hashes. withdrawals is absent in both.

If no era1 file is at hand, any epoch with a downloadable era1 file works; the choice is not
load-bearing beyond "pre-merge, aligned, non-trivial".

### 6.3 Headline queries, by name

1. Total gwei withdrawn in relic 2079, with the count of withdrawals and the first withdrawal's
   block and validator index.
2. Exact ERC-20 transfer volume of one named token across the relic, from `logs.data` as a
   uint256, summed exactly.
3. The same shape over Approval events, where unlimited approvals (2^256 minus 1) make the exact sum
   impossible in DECIMAL(38,0): the statement that shows refusal and `_overflow` rather than a
   wrong number.

Exact SQL and expected results are §12; what a reader can check them against is §12.4.

## 7. Shadow archive-RPC source

The source is a pipeline of five stages with a durable journal between fetch and seal. Everything
after the journal is the same code the era1 source already runs: canonical rows from legacy-format,
Parquet from legacy-parquet, manifest and pact from legacy-format. The RPC source owns only how rows
are obtained and checked before they reach that path.

### 7.1 Stages

1. **Plan.** `shadow plan --from --to` already prints relic coverage. The RPC source extends it
   with the request count per relic (2 methods per block, so 16,384 for a full relic, plus probes)
   and the estimated CU for a profile that bills, without touching the network.
2. **Probe.** Against a provider profile: `eth_chainId` must equal the configured chain id;
   `eth_getBlockReceipts` on one block inside the range must return a non-empty array for a block
   known to have transactions; `eth_getBlockByNumber(n, true)` on the same block must return full
   transaction objects; `eth_getBlockByNumber('finalized')` must be at or beyond relic.end. A failed
   probe stops before any block is fetched. Probes count against `--budget-requests`.
3. **Fetch and journal.** For each block in the relic: `eth_getBlockByNumber(n, true)` and
   `eth_getBlockReceipts(n)`, with bounded concurrency and a token bucket (§7.3). A response is
   validated before it is journaled (§7.2) and discarded, not journaled, if validation fails. The
   journal is append-only and keyed by block number, so resumption is "which blocks are missing
   from the journal".
4. **Decode to rows.** Journaled responses become the canonical row types for headers,
   transactions, receipts, logs and withdrawals, in the normative sort order. Logs are derived from
   receipts; `log_index` is the block-level index the receipt carries.
5. **Seal.** The staged rows go through the shared writer profile, the manifest is built with
   `prev_relic_manifest_hash` from `--after`, and the relic lands in `--out` exactly as the era1
   source does. Nothing in the manifest records the provider; `provenance.json` beside the journal
   does (P4).

A response that fails validation never reaches the journal; a crash loses nothing the journal
holds; stage 5 is the era1 source's sealer unchanged.

### 7.2 Validation before journaling

A block response is accepted into the journal only if all hold:

- `number` equals the requested block and `hash` is non-empty.
- `parentHash` equals the hash of block n minus 1 as already journaled, or for the relic's first
  block, the hash fetched for relic.start minus 1 (stored as `parent_hash_of_start`).
- The header fields reconstruct to the stated hash through the chain ID 1 header RLP layouts
  legacy-format already has. This is the same check the cleaner performs later; doing it at fetch
  time means a provider serving a wrong header is caught before paying for its receipts.
- Every transaction object reconstructs to an EIP-2718 envelope whose keccak equals its hash, for
  types 0 through 4 (type 0 unprotected and EIP-155 handled per RFC-0001 §6.4 parity
  normalization). Reconstruction failure for an unsupported type is a hard stop for the relic, not
  a silent null.
- `from` on each transaction equals the sender recovered from the signature (k256 is already a
  Shadow dependency). The cleaner does not check this; the Shadow does, and the fact is recorded in
  the run report as a Shadow-level check, not a cleaning claim.

A receipts response is accepted only if the array length equals the block's transaction count,
`transactionHash` and `transactionIndex` match position for position, `cumulativeGasUsed` is
non-decreasing and ends at the header's `gasUsed`, and the OR of receipt blooms equals the header
bloom. An empty array for a block with transactions is a provider error, never an empty block.

`gas_used` per receipt is derived from consecutive `cumulativeGasUsed` values. `effective_gas_price`
is taken from the receipt. Withdrawals come from the block response's `withdrawals` array with the
block-level ordering as the trie key and the global index stored.

### 7.3 Rate and cost model against the GraphOps endpoint

The endpoint is free to Pete, so the CU budget is zero-priced; the request budget still bounds the
run. Unknowns to measure in the first probe run, recorded in the provider profile: requests per
second allowed, whether JSON-RPC batches are accepted and how their members are counted, and the
response time for a Shapella-era block with receipts.

Estimates, labelled as such: a 2023 mainnet block carries roughly 150 to 200 transactions; the
block-with-transactions JSON plus its receipts is on the order of 1 to 2 MB. A full relic is
therefore roughly 8 to 16 GB of JSON over the wire and, zstd-compressed in the journal, roughly 2 to
4 GB on disk. At 8 concurrent requests and 100 to 300 ms per response the fetch is tens of minutes;
at a 10 request per second limit it is about half an hour. None of these numbers is a measurement.

The journal stays local and can be deleted after sealing; a relic re-seal from the journal needs no
network, which is what makes writer-profile experiments cheap.

### 7.4 Determinism and resumability

Determinism is defined by RFC-0001 §11.1 at the content-hash level, and the oracle relic (§6.2) is
how it is shown: era1 and RPC productions of relic 209 must agree table by table. Known places the
two paths can differ, each of which must agree after decoding: sender recovery versus `from` as
served; `raw_envelope` present in both (RPC reconstructs, era1 carries) but excluded from content
hashing by spec; `transaction_index` and `log_index` numbering.

Resumption is a restart with the same `--out` and journal directory: the ingester lists missing
blocks, re-probes finality once, and continues. Nothing is re-fetched that the journal holds. A
sealed relic directory is never written into twice; a resumed run that reaches a complete journal
writes the relic to a fresh `--out`.

### 7.5 CLI

```
shadow seal --source rpc --from 17031168 --to 17039359 \
  --endpoint $GRAPHOPS_RPC --profile profiles/graphops.toml \
  --journal ./journal/1/002079 --out ./relics/1/002079 \
  --after ./relics/1/002078/manifest.json \
  --budget-requests 20000 --concurrency 8 --rps 10
```

`--after` is required for any relic after genesis. For the showcase relic there is no predecessor on
disk, so the run needs a decision recorded in §15: either seal 2078 first (another full relic) or
allow a documented "detached" pact start whose `prev_relic_manifest_hash` is the all-zero genesis
value. The second is cheaper and is what the RFC proposes for the first publication, with the
registry naming the gap. (Amended: see the note at the top; the flag is `--detached`.)

## 8. Sealing

Sealing is RFC-0001 §6, §8 and §11.1 applied through the crates that already implement them. This
section lists only what the RPC source must get right on the way in, and the two decisions it
forces.

### 8.1 Rows and tables

- **headers:** all 23 columns from the block response. difficulty is 0 and total_difficulty null
  post-merge; `withdrawals_root` present from the Shapella block onward and null before it within the
  same relic. `extra_data` stays variable-length.
- **transactions:** all 25 columns. `raw_envelope` is stored (default on for the RPC source per
  §6.4) and is the reconstructed EIP-2718 envelope whose keccak was checked at fetch time.
  `v_or_y_parity` follows the normalization rule; `chain_id` is null for unprotected legacy
  transactions. OP-stack columns are null.
- **receipts:** status set, `post_state` null (all post-Byzantium), `gas_used` derived,
  `effective_gas_price` from the receipt, blob and OP columns null for relic 2079.
- **logs:** flattened from receipts, `log_index` block-level, topics as a contiguous prefix,
  `removed` never stored.
- **withdrawals:** empty for blocks before activation, populated after, ordered by
  `(block_number, index)`.

The staged rows are held in memory per relic. At roughly 1.4 million transactions and several
million logs for a 2023 relic this is well within the 64 GB machine the first run uses, and the
era1 sealer already works this way; a streaming sealer is RFC-0002 P2 territory and out of scope.

### 8.2 Manifest and pact

`files` carries the five core tables with per-file blake3, content_hash, row_count and row_groups as
the writer reports them. `era1_accumulator_root` is absent (post-merge). `producer` is absent for
the first publication; adding a secp256k1 signature is a one-line follow-up once the key handling is
decided. `prev_relic_manifest_hash` depends on the §7.5 decision: either the hash of a sealed 2078
or the all-zero genesis value with the registry recording that the pact starts at 2079.

Decision proposed: the first published pact for Silo 1 starts at relic 2079 with a zero
predecessor. The registry says so in a top-level coverage note (RFC-0001 §8.5 already allows the
registry to describe gaps). When earlier relics are sealed later they form their own pact segment
until the chain is joined by a re-seal of 2079 with the real predecessor, which changes 2079's
manifest hash but not its content hashes. The post states that the published 2079 is a detached
pact start. (Amended: RFC-0001 did not allow this as drafted; §8.4.1 now does, with the flag
`detached_pact_start` inside the manifest hash.)

### 8.3 Output layout

```
relics/1/002079/
  manifest.json
  headers.parquet
  transactions.parquet
  receipts.parquet
  logs.parquet
  withdrawals.parquet
```

No `idx/` sidecars in this RFC. The relic directory is written once and never modified; a re-seal
goes to a new directory.

## 9. Cleaning

The relic is cleaned with the tool that exists, and its report is the only verification claim the
post makes.

```
cargo run -p solo -- clean --files --json relics/1/002079/manifest.json > reports/002079.json
```

### 9.1 What the report establishes, for relic 2079 on chain ID 1

| Check | Covered by `solo clean --files` today |
|---|---|
| File sizes and BLAKE3 against the manifest | yes |
| Parquet footer counts, schema, row order, content hashes, decoded counts and block bounds, all five tables | yes |
| Headers cover every block, stored parent links consistent, boundary matches manifest | yes |
| Header RLP and Keccak reconstruction through Prague | yes (chain ID 1 only) |
| Transaction trie root from raw envelopes, envelope hashes | yes |
| Receipt trie root, receipt and header blooms, contiguous receipt indices, cumulative gas, block gas limit | yes |
| Log to transaction and receipt references | yes |
| Withdrawal trie roots (EIP-4895) | yes, where the table exists |
| era1 accumulator | not applicable, no root in a post-merge manifest |
| Transaction field agreement with raw envelope, signatures, sender recovery | no |
| Fees and other derived fields | no |
| Fork activation, consensus rules | no |
| Canonical-chain membership, checkpoint trust | no |

### 9.2 What this RFC adds and does not add

Nothing is added to the cleaner. Two of the "no" rows above are checked by the Shadow at fetch time
(envelope hash, sender recovery, §7.2); the run report of `shadow seal` says so under a separate
heading, because a Shadow's claim about its own input is not a cleaning claim about a published
file. A reader who downloads the relic can repeat the cleaning; they cannot repeat the Shadow's
fetch-time checks without the journal, and the post must not blur the two.

The withdrawal-root check is the one that matters most for relic 2079 and it is the newest in the
cleaner. If it reports anything other than a clean pass on the real relic, that is the first bug
this RFC finds, and it is reported as such before any publication.

### 9.3 Oracle comparison

For relic 209 the check is a table-by-table comparison of `content_hash` between the era1 manifest
and the RPC manifest, plus a full `solo clean --files` of both. A mismatch is diagnosed by diffing
the canonical rows (both sealers can dump them) rather than by trusting either side. The expected
places for a first mismatch are sender recovery and the legacy v normalization; both are Shadow bugs
to fix, never spec changes.

## 10. Publication

The relic is published under the RFC-0001 §9.1 path convention on one bucket with public read, and
the registry is written last.

```
<bucket>/legacy/v1/1/relics/002079/manifest.json
<bucket>/legacy/v1/1/relics/002079/headers.parquet
<bucket>/legacy/v1/1/relics/002079/transactions.parquet
<bucket>/legacy/v1/1/relics/002079/receipts.parquet
<bucket>/legacy/v1/1/relics/002079/logs.parquet
<bucket>/legacy/v1/1/relics/002079/withdrawals.parquet
<bucket>/legacy/v1/1/registry.json
```

### 10.1 Choices

- **Storage:** any S3-compatible bucket with range reads and public GET. Cloudflare R2 is the
  proposed first host because egress is free, which matters for a file people are invited to
  download; the choice is operational and not part of the spec. Bucket name and account are not
  recorded in any manifest.
- **Mirroring:** the relic directory is also committed as a GitHub release asset on
  nuthatch-org/the-legacy, so a reader without S3 tooling can curl it. The release notes carry the
  manifest hash and the cleaning report.
- **Naming:** relic index zero-padded to six digits, as the spec's example shows (002079).
- **Hashes in the open:** the post, the release notes and `registry.json` each carry the manifest
  BLAKE3. A reader checks it with `b3sum manifest.json` before anything else.
- **Size:** a 2023 mainnet relic with raw envelopes is estimated at 2 to 4 GB of Parquet; measured
  once sealed and stated in the registry, not guessed in the post.

### 10.2 How a reader fetches it

```
curl -O <base>/legacy/v1/1/relics/002079/manifest.json
b3sum manifest.json                      # matches the published hash
cargo run -p solo -- clean --files manifest.json   # after fetching the five tables
```

For Burrmill the fetch is not needed: `open_relics` takes the mirror URL and reads Parquet footers
by range through `object_store`, exactly as `open_store` does for nests today (§11.6).

### 10.3 What the registry says

`registry.json` lists relic 2079 with its manifest hash, the pact root at that height, and a
coverage note stating that Silo 1 currently contains one detached relic and that lower relics are
not yet sealed. An empty registry has no sealed head (RFC-0002 §3); a registry with one detached
relic has a sealed head of 17,039,359 and a sealed range that is not contiguous from genesis, and
Solo's capability advertisement must say so if Solo is ever pointed at it.

## 11. Burrmill relics provider

Burrmill gains one catalogue entry point, `Engine::open_relics`, that admits a relic corpus the way
`open_nest` admits a nest: through its own provider, under the RFC-0044 §3.6 allowlist, with no file
function reachable from SQL. The dependency runs one way: Burrmill depends on legacy-format and
legacy-parquet (and may use legacy-reader for verification); nothing in The Legacy depends on
Burrmill.

### 11.1 Crate boundary and feature

- New Burrmill feature `relics`, off by default, pulling legacy-format, legacy-parquet and
  object_store. RFC-0045 cares about footprint; the feature keeps the relic code out of nuthatch's
  default graph until nuthatch asks for it.
- Arrow and parquet are already pinned to major 59 on both sides; the relic Parquet files use zstd,
  so Burrmill's parquet dependency gains the zstd feature under `relics`. Its size cost is measured
  and recorded per RFC-0045 §2.6.
- Versions: Burrmill pins a git revision of the-legacy's format crates, as nuthatch pins Burrmill.
  No crates.io publish is needed for the first run.

### 11.2 Opening and admission

`open_relics(root)` where root is a local directory holding `relics/<index>/manifest.json` entries,
or `open_relics_store(store, prefix)` for a bucket or HTTP mirror. Admission per relic:

1. Parse the manifest with legacy-format; refuse a manifest whose `files` names are not the five
   canonical table names (the spec's traversal rule).
2. Check each file's byte size and, locally, its BLAKE3 against the manifest. Over an object store
   the hash check is optional and off by default because it means reading the whole object; the
   `relics` table records whether it ran.
3. Check the pact linkage across the admitted set when more than one relic is present.
4. Record the result per relic in the `relics` metadata table (§11.3). A relic that fails admission
   is listed there with its failure and exposes no rows.

No trie, header or bloom checks run at open. Those are `solo clean`'s job, and the post says which
report the published relic passed. Burrmill's claim is narrower: the bytes it reads are the bytes
the manifest names.

### 11.3 Tables

| SQL table | Source | Notes |
|---|---|---|
| headers | headers.parquet across admitted relics | union by name, pruned by relic range |
| transactions | transactions.parquet | `raw_envelope` exposed as binary; usually projected away |
| receipts | receipts.parquet | |
| logs | logs.parquet | address and topic predicates pushed to row-group statistics |
| withdrawals | withdrawals.parquet | empty when the table is absent in every admitted relic, never an error |
| relics | manifests | one row per relic: index, start, end, manifest BLAKE3, pact root, admitted, bytes_checked, failure |

Each table is one TableProvider whose scan is a parquet-rs read with projection and `block_number`
range pushdown, partitioned by relic. `block_number` predicates prune relics before any footer is
read, which is what makes the object-store path cheap for a one-relic question.

### 11.4 Type mapping for EVM scalars

This is where the pairing has a point. Relic uint256 columns are minimal big-endian byte magnitudes
(RFC-0001 §6.3). Burrmill's exact integer is DECIMAL(38,0), which holds values up to about 1.0e38,
while uint256 goes to about 1.16e77. The provider therefore exposes, for every uint256 column `c`:

- `c`: the raw binary, always.
- `c_dec`: DECIMAL(38,0), the exact value when it fits, NULL when it does not. Computed by a
  provider-owned conversion from the byte magnitude, not by TRY_CAST from text, because there is no
  text.
- `c_overflow`: boolean, true exactly when `c` is non-null and `c_dec` is NULL.

This is the nuthatch word32 convention applied to a different encoding, so every rule Burrmill
already has for `_dec` and `_overflow` (RFC-0044 §3.2, the TRY_CAST taint in the checked-arithmetic
rewrite) applies unchanged. Summing `c_dec` over rows where `c_overflow` is true is a NULL-tainted
sum that the rewrite refuses rather than silently drops; a reader who wants the sum over the
representable rows writes the filter explicitly. The §12 Approval query is built on exactly this.

Other mappings: uint64 columns stay UBIGINT; uint32 and uint8 stay UINTEGER and UTINYINT; fixed
binaries (hashes, addresses, blooms) stay BLOB, with a `hex(c)` scalar for display; `block_number`
is the join and pruning key everywhere. `logs.data` is BLOB and gets no `_dec` of its own; the ERC-20
value is the first 32-byte word of data, so the provider adds `word(data, n)` returning the n-th
32-byte word as a uint256 binary, which then takes the same `_dec` conversion through a scalar
`u256_dec(b)`. These two scalars are the only functions the feature registers.

### 11.5 Admitted subset

Nothing new. The showcase statements are filters, projections, a group-by with SUM and COUNT, and
MIN on `block_number`: shapes Burrmill admits today. If a relic column type makes a planner rule
misfire, that is a Burrmill bug and is filed as one; the admitted subset is not widened for this
RFC.

### 11.6 Object store path

`open_relics_store` mirrors `open_store`: footers read by range, row groups read on demand, no local
copy. The same showcase statement run against the local directory and against the mirror must
return identical rows; burrmill-bench gets a `relics-parity` subcommand that asserts it.

## 12. Showcase queries

Three statements, each answering one of the §6.3 questions, each with an independent cross-check.
Expected values are filled in by the run, never typed into the post from memory; the post ships only
when every cross-check matches.

### 12.1 Withdrawals start inside the relic

```sql
SELECT COUNT(*)            AS withdrawals,
       SUM(amount)         AS gwei_total,
       MIN(block_number)   AS first_block,
       MIN("index")        AS first_index
FROM withdrawals
WHERE block_number BETWEEN 17031168 AND 17039359;
```

Expected: `first_block` is 17,034,871, the block after the Shapella activation block (amended: the
activation block 17,034,870 carries an empty withdrawals list), `first_index` is 0, and `gwei_total`
is a UBIGINT sum with no `_dec` involved, because `amount` is uint64 by spec. A second statement
groups by `block_number` for the first ten blocks after activation so the reader can see
withdrawals appear.

### 12.2 Exact ERC-20 transfer volume

Token: stETH, `0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84`, chosen because the Shapella relic is the
week its withdrawals story began and its transfers are dense. `Transfer(address,address,uint256)`
has topic0 `0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef`.

```sql
SELECT COUNT(*)                                   AS transfers,
       SUM(u256_dec(word(data, 0)))               AS wei_total,
       SUM(CASE WHEN u256_dec(word(data, 0)) IS NULL THEN 1 ELSE 0 END) AS overflow_rows
FROM logs
WHERE address = X'ae7ab96520de3a18e5e111b5eaab095312d7fe84'
  AND topic0  = X'ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef'
  AND block_number BETWEEN 17031168 AND 17039359;
```

Expected: `overflow_rows` is 0 (no single stETH transfer approaches 1e38 wei), so `wei_total` is
exact. If `overflow_rows` is not 0 the sum is NULL-tainted and the checked-arithmetic rewrite
refuses the statement; that outcome, should it occur, is itself the demonstration and is reported
as what happened.

### 12.3 Where exactness has to refuse

`Approval(address,address,uint256)`, topic0
`0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925`. Unlimited approvals are 2^256
minus 1, far above DECIMAL(38,0).

```sql
-- 1. the refused statement, shown with its error
SELECT SUM(u256_dec(word(data, 0))) AS approved_total
FROM logs
WHERE address = X'ae7ab96520de3a18e5e111b5eaab095312d7fe84'
  AND topic0  = X'8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925'
  AND block_number BETWEEN 17031168 AND 17039359;

-- 2. the honest statement
SELECT COUNT(*) FILTER (WHERE u256_dec(word(data, 0)) IS NULL)     AS unlimited_or_oversized,
       COUNT(*) FILTER (WHERE u256_dec(word(data, 0)) IS NOT NULL) AS representable,
       SUM(u256_dec(word(data, 0))) FILTER (WHERE u256_dec(word(data, 0)) IS NOT NULL) AS representable_total
FROM logs
WHERE address = X'ae7ab96520de3a18e5e111b5eaab095312d7fe84'
  AND topic0  = X'8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925'
  AND block_number BETWEEN 17031168 AND 17039359;
```

The point made in the post: statement 1 refuses; statement 2 says how many rows could not be
represented and sums the rest exactly. The claim is about Burrmill's own behaviour. No claim is made
about what DuckDB or any other engine returns on the same input unless it is run and transcribed in
the same post. (The FILTER clause is used if Burrmill's dialect admits it; otherwise statement 2 is
written as a CASE form, which it certainly admits.)

### 12.4 Cross-check procedure

| Statement | Independent source | Method |
|---|---|---|
| 12.1 | Any archive RPC, `eth_getBlockByNumber` with withdrawals, for the relic range | A 60-line Python script summing amount over 8192 blocks; published with the post |
| 12.1 first withdrawal | A block explorer page for the first withdrawal block | Visual check, linked |
| 12.2 | `eth_getLogs` over the same address and topic, summed with Python integers | Same script; Python integers are unbounded, so the two sums must agree bit for bit |
| 12.3 | Same `eth_getLogs` | The script counts values above 10^38 minus 1 and compares with `unlimited_or_oversized` |

The script fetches from the same GraphOps endpoint, which means the cross-check shares a provider
with the Shadow. That is stated. A reader with a different provider can rerun it; the relic is the
thing that does not change.

## 13. Testing and acceptance gates

Each milestone in §14 has one gate. A gate that fails stops the sequence and is written up, per
Burrmill's "a no is a result" rule (RFC-0044 §12).

| Milestone | Gate | Evidence |
|---|---|---|
| M1 Fixture ingester | Against a local HTTP fixture server serving canned responses: budgets exhaust and stop, an unavailable `eth_getBlockReceipts` fails the probe, out-of-order batch replies are reassembled, a corrupted response is rejected and not journaled, a crash mid-relic resumes without re-fetching | `cargo test -p shadow` with the fixtures in-repo; no network in CI |
| M2 Oracle relic | Relic 209 from era1 and from RPC: equal `content_hash` on headers, transactions, receipts, logs; both pass `solo clean --files` | Both manifests and both reports committed under `docs/evidence/0004/` |
| M3 Showcase relic sealed | Relic 2079 sealed within `--budget-requests`; `solo clean --files --json` passes every chain ID 1 check including withdrawal roots | The JSON report committed; the run report with request counts and wall time |
| M4 Published | Manifest hash in `registry.json`, release notes and post agree; b3sum of the fetched manifest matches; a fresh clone can `solo clean --files` the downloaded relic | A second machine (or a clean container) repeats the fetch and clean |
| M5 Burrmill provider | `open_relics` admits the local relic and the mirror; `relics` table shows one admitted row; the five tables return the manifest's `row_count` on `COUNT(*)`; `relics-parity` passes local versus mirror | `cargo test -p burrmill --features relics`; the bench transcript under `docs/bench/` |
| M6 Showcase | The three §12 statements return values that match the cross-check script bit for bit; statement 12.3 part 1 refuses with a readable error | Transcript of both sides, committed with the script |
| M7 Post | Every verification sentence in the post is traceable to a line of a committed report; the traces and canonical-chain non-claims are present | A reviewer (a second pair of eyes, or Claude in a fresh session) checks the post against §4.3 before it goes out |

Additional tests that do not gate but are written:

- Envelope reconstruction round trips: for each transaction type 0 to 4, build the envelope from
  RPC-shaped fields, hash it, decode it with legacy-format's existing decoders, compare field by
  field. Vectors taken from real transactions in the journal once M3 exists, plus the synthetic ones
  already in the repo.
- Sender recovery agreement with `from` across the whole relic, as a Shadow report line (count of
  transactions checked, count of disagreements, which must be 0).
- Burrmill `u256_dec` on the boundary values: 0, 1, 10^38 minus 1 (representable), 10^38 (NULL),
  2^256 minus 1 (NULL), and a 33-byte magnitude (refused as malformed, never NULL).

## 14. Work plan

The long pole is the Shadow; the Burrmill provider is small and can start as soon as any real relic
exists, which the era1 oracle relic provides before the RPC run. Estimates are in evenings of
focused work and are an order of magnitude, in the spirit of Burrmill's ROADMAP note that §7
estimates were wrong in both directions.

| # | Milestone | Depends on | Estimate | Notes |
|---|---|---|---|---|
| M1 | Fixture ingester: journal, budget, probe, resume, decode to rows, seal through the shared path | nothing | 5 to 8 evenings | The bulk is envelope reconstruction and the fixture server; the journal and budget are plain |
| M2 | Oracle relic 209 from era1 and RPC, content hashes equal | M1, an era1 file | 1 to 3 evenings | Expect at least one Shadow bug in sender recovery or legacy v; that is the point of M2 |
| M3 | Showcase relic 2079 sealed and cleaned | M2, provider profile for GraphOps | 1 to 2 evenings plus the run | The run itself is wall-clock, not effort |
| M4 | Published on R2 and as a release asset, registry written | M3 | 1 evening | Includes the second-machine re-clean |
| M5 | Burrmill relics feature: provider, relics table, `u256_dec` and `word`, object store path, parity bench | the era1 relic from M2 (can start before M3) | 4 to 6 evenings | Runs in parallel with M3 and M4 |
| M6 | Showcase statements and the cross-check script, results matched | M4, M5 | 1 to 2 evenings | |
| M7 | Post, reviewed against §4.3 | M6 | 1 evening | |

Burrmill's lane starts when the oracle relic exists, so the provider is tested on real Parquet while
the RPC run is still fetching; the second handoff is the published mirror URL.

Total on the critical path (M1 to M4, then M6, M7): roughly 10 to 17 evenings; M5 overlaps. Nothing
here needs a paid endpoint or a synced node.

Order of commits in the-legacy: `pete/rpc-journal`, `pete/rpc-envelopes`, `pete/rpc-seal`,
`pete/oracle-209`, `pete/relic-2079`, each single-line. In burrmill: `pete/relics-provider`,
`pete/u256-dec`, `pete/relics-store`.

## 15. Risks and open questions

### 15.1 Risks

| Risk | Likelihood | Effect | Mitigation |
|---|---|---|---|
| GraphOps endpoint lacks `eth_getBlockReceipts` or serves it only for recent blocks | medium | M3 blocked | Probe first (§7.1). Fallback is per-transaction `eth_getTransactionReceipt`, which is 1.4 million requests for relic 2079 and must be explicitly budgeted, per P4. Second fallback: a different free provider for the receipts half, recorded in `provenance.json` |
| Rate limit lower than expected | medium | M3 takes hours, not minutes | Token bucket honours it; the journal makes a multi-session run fine |
| Envelope reconstruction mismatch for some type 1 or 2 transactions (access list ordering, yParity versus v fields differing by provider) | medium | hard stop at fetch | M1 fixtures include each type; the first real mismatch is diagnosed against the journal, which holds the raw response |
| Sender recovery disagrees with served `from` for a few transactions | low | M2 oracle mismatch | Both the era1 and RPC paths recover; a disagreement is a Shadow bug and gets a vector in legacy-format |
| Withdrawal root check fails on real data | low | M3 blocked | This would be a real cleaner bug and is the best possible outcome of M3 short of a pass; fixed in the cleaner with the RFC-0001 §10.3 text amended if needed |
| Relic memory footprint during sealing exceeds the machine | low | M3 blocked | 64 GB machine; 2023 relic rows are estimated under 10 GB decoded. If wrong, seal in two passes per table from the journal |
| Burrmill checked-arithmetic rewrite handles DECIMAL(38,0) from a binary-sourced scalar differently from TRY_CAST text | medium | 12.3 does not refuse the way it should | The §13 boundary tests exist for this; if the rewrite needs a taint source for a UDF, that is a Burrmill change filed on its own |
| FILTER not in Burrmill's admitted dialect | high | cosmetic | CASE form, already noted in §12.3 |
| Detached pact start confuses readers | medium | reputational | §8.2 and §10.3 say it plainly; the registry says it; the post says it |
| The post overstates verification | always present | the one failure that matters | M7 gate reviews every sentence against §4.3 |

### 15.2 Open questions

- [x] Predecessor policy: detached pact start at 2079 versus sealing 2078 first. Decided 2026-11-05:
  detached, under RFC-0001 §8.4.1.
- [ ] Whether producer signatures are added for the first publication, and with which key.
  Proposed: not yet.
- [ ] Bucket: R2 under Nixum Ltd or under a nuthatch-org account. Operational; decide before M4.
- [ ] Does Burrmill's relics feature live in `crates/burrmill` behind a feature or in a new
  `crates/burrmill-relics`? Proposed: a feature, revisited if RFC-0045's footprint gate objects.
- [ ] Whether legacy-reader should grow an Arrow RecordBatch surface so Burrmill does not reparse
  Parquet itself. Proposed: no for this RFC; Burrmill reads Parquet directly with pruning, and the
  reader stays row-oriented until RFC-0002 P2 says otherwise.
- [ ] Second showcase relic: Dencun (2371) for blobs, or the merge boundary (1896) to settle the
  partial-accumulator handling. Not for this RFC.

## 16. Appendix

### A. RPC methods used, per block

| Method | Params | Used for | Validated against |
|---|---|---|---|
| `eth_chainId` | none | probe | configured chain id |
| `eth_getBlockByNumber` | `"finalized", false` | probe, resume | number >= relic.end |
| `eth_getBlockByNumber` | `n, true` | headers, transactions, withdrawals | header hash reconstruction, envelope hashes, sender recovery, parent link |
| `eth_getBlockReceipts` | `n` | receipts, logs | count, position, cumulative gas, bloom OR against header |

No `eth_getLogs` in the Shadow: logs come from receipts, which is both cheaper and complete by
construction. `eth_getLogs` appears only in the cross-check script.

### B. Journal layout

```
journal/1/002079/
  profile.toml         # provider profile as probed, keys redacted
  provenance.json      # provider alias, probe results, budget accounting
  blocks/
    17031168.json.zst  # {block: <eth_getBlockByNumber result>, receipts: <eth_getBlockReceipts result>}
    ...
  index.bin            # bitmap of journaled block numbers, rebuilt from blocks/ on open
```

One file per block, compressed, written atomically (temp file then rename). `index.bin` is a cache;
the directory listing is the truth.

### C. Provider profile (TOML)

```toml
[provider]
alias = "graphops"              # never the URL
chain_id = 1

[methods]
get_block_by_number = true
get_block_receipts = true        # probed
get_transaction_receipt = true
batch_max = 0                    # 0 = batches not used

[limits]
requests_per_second = 10         # probed or configured
concurrency = 8

[billing]
unit = "request"                 # or "cu"
cost_per_block = 2               # two methods per block
```

### D. Manifest excerpt, relic 2079 (shape only, values from the run)

```json
{"spec_version":1,"chain_id":1,"silo":"Silo 1",
 "block_range":{"start":17031168,"end":17039359},
 "boundary":{"start_block_hash":"...","end_block_hash":"...","parent_hash_of_start":"..."},
 "blocks_per_relic":8192,
 "files":[{"name":"headers.parquet","table":"headers","byte_size":0,"blake3":"...","content_hash":"...","row_count":8192,"row_groups":0}, "...four more..."],
 "prev_relic_manifest_hash":"0000...0000",
 "pact_root":"...","hash_algo":"blake3"}
```

JCS-canonical on disk; shown here with whitespace for reading only.

### E. Burrmill surface sketch

```rust
// burrmill, feature = "relics"
impl Engine {
    pub fn open_relics(root: &Path) -> Result<Self>;
    pub async fn open_relics_store(store: Arc<dyn ObjectStore>, prefix: &str, threads: usize) -> Result<Self>;
}

// Registered by the feature, and nothing else:
//   word(data: BLOB, n: INTEGER) -> BLOB      the n-th 32-byte word of an ABI-encoded data blob
//   u256_dec(b: BLOB) -> DECIMAL(38,0)        exact when <= 10^38 - 1, NULL above, error on > 32 bytes
//   hex(b: BLOB) -> VARCHAR                   lowercase, no 0x

// Tables: headers, transactions, receipts, logs, withdrawals, relics.
// Every uint256 column c also has c_dec and c_overflow, as nuthatch's word32 columns do.
```

### F. Cross-check script, outline

```
# check_2079.py  - python3 -I check_2079.py <rpc-url>
# 1. for n in 17031168..17039359: eth_getBlockByNumber(n, False) -> sum withdrawals[].amount, first block with any
# 2. eth_getLogs(address=stETH, topics=[Transfer], fromBlock, toBlock) in 1000-block windows
#    -> count, sum(int(data[:66], 16)) with unbounded ints
# 3. same for Approval -> count values > 10**38 - 1, count and sum the rest
# prints the three results in the same column names as the SQL
```

The script is committed next to the evidence and its output transcript next to the Burrmill
transcript, so the "bit for bit" claim in §13 is checkable by diffing two files.
