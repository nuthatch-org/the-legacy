# RFC-0002: The Backfill Layer - Follow-up Improvements So Any Indexer Can Stop Paying for History

- **RFC:** 0002
- **Status:** Draft, proposed interfaces and amendments, not an implementation claim
- **Author:** Pete (Petko Pavlovski)
- **Date:** 2026-09-26
- **Repo:** github.com/nuthatch-org/the-legacy
- **Depends on:** [RFC-0001](0001-the-legacy.md), normative except where amendments in §8 are adopted

## 1. Abstract

RFC-0001 specifies the corpus, its verification, and Solo as a splitting proxy behind erpc/proxyd.
This RFC asks what an indexer needs to backfill from The Legacy instead of a paid history provider,
and which costs remain afterwards. Sealed history is reusable across indexers; the unsealed range,
historical state, and traces outside the optional tier still need other sources.

Seven proposals follow: sealed-only serving and capability advertisement (P1), a native Rust reader
(P2), transaction address sidecars and `legacy_activeBlocks` (P3), cost-bounded archive-RPC production
(P4), a usable-corpus-first roadmap (P5), Porter as the sibling tip fan-out (P6), and a public-mirror
operating model (P7). These are proposed work. The current implementation status lives in the
[README](../../README.md), not in the acceptance criteria below.

## 2. Motivation

### 2.1 The bill, decomposed

An isolated cursor repeatedly fetching the same head and logs pays for work shared with every
other cursor on that chain. The motivating Nuthatch estimate is approximately $75/month per
cursor. That is a workload-specific input, not a measured result reproduced in this repository.
Polling cadence, method mix, retries and windows determine the bill; Arbitrum must not be assumed
to have Base's block cadence.

For historical reads, use the illustrative rate **$0.525 per million CU** from Alchemy's current
[pricing page](https://www.alchemy.com/pricing). Its older
[PAYG FAQ](https://www.alchemy.com/docs/reference/pay-as-you-go-pricing-faq) still lists different,
tiered rates; the estimates here use the pricing page, not that FAQ. Method costs come from the
[CU table](https://www.alchemy.com/docs/reference/compute-unit-costs): block reads and block receipts
are 20 billing CU each, `eth_getLogs` is 60, and batches sum their members. Throughput CU are a
separate limit; notably block receipts are listed at 500 throughput CU, not 20.

At those assumptions, 50M individual block reads cost $525. A logs scan costs
`ceil(blocks / effective_window) * 60 * 0.525 / 1_000_000`, before retries: approximately $0.016 at
a 100,000-block window or $1.575 at a 1,000-block window. A sparse address is not itself a guarantee
of a wide provider window. These are arithmetic estimates, not paid runs. Throughput add-ons,
taxes, discounts, failed attempts and storage are excluded. Adding a contract can repeat the
backfill unless the raw history has been retained locally.

### 2.2 What RFC-0001 already solves

| Indexer need | RFC-0001 | Gap this RFC closes |
|---|---|---|
| Finalized JSON-RPC reads and log semantics | §13.3, §13.5 | sealed-only operation and limits (P1) |
| Deployment behind erpc/proxyd | §13.8 | none |
| Object-storage corpus and mirrors | §9, §10 | mirror workflow (P7) |
| era1, Reth, Erigon, Firehose, ExEx sources | §11 | cost-bounded RPC producer (P4) |
| A node-free history endpoint | upstream assumed in §13.2 | P1 |
| Native Rust reads | no reader API specified | P2 |
| Blocks with matching logs | §7.1 pruning sidecars | exact block lookup (P3) |
| Blocks with matching transaction sender/recipient | absent | P3 |
| Unsealed range | forwarded, §3.2 | sibling and handoff (P6) |
| Corpus before a synced local producer node | §18 starts with Reth | P5 |
| Nuthatch as consumer | §17 | P2 and P7 |

### 2.3 Adoption

JSON-RPC compatibility is the first path for graph-node, ponder, rindexer, shovel and scripts.
Changing a URL is sufficient only for workloads within the supported history methods: an indexer
that also needs state, subscriptions, traces or provider-specific methods requires another source.
Rust consumers can avoid the JSON-RPC hop by using the reader directly. Compatibility with each
named indexer is an acceptance test to run, not an established fact.

## 3. Terminology

- **Sealed head:** the highest contiguous block available from a published registry snapshot.
  An empty registry has no sealed head; block zero must not stand in for missing coverage.
- **Served head:** the sealed head of the snapshot a particular mirror has fetched and admitted
  under its configured cleaning policy. A remote registry may be ahead of this mirror.
- **Sealed-only mode:** Solo with no upstream, serving only its admitted corpus snapshot.
- **Reader:** `legacy-reader`, the proposed storage and query layer, independent of JSON-RPC.
- **Active blocks:** blocks containing matching log emitters or top-level transaction `to`/`from`.
  This is not a complete set of state changes, internal calls or native transfers for an address.
- **Porter:** provisional name for the unsealed-range fan-out component, specified later in RFC-0003.

## 4. Proposals

### P1. Sealed-only serving and capability advertisement

`[upstream]` becomes optional. Without it, the advertised boundary is the local served head,
refreshed by admitting a complete new registry snapshot. Each request, including every member of
an HTTP batch, uses one snapshot. A mirror never advertises an unfetched or unchecked extension.

`eth_blockNumber` returns that head. The `latest`, `safe` and `finalized` tags resolve to it, which
must be documented as the mirror's historical view, not the live network head. `eth_chainId` comes
from the registry and `eth_syncing` is false once a corpus is ready. Startup with no admitted relics
is not ready and history/head calls return an explicit unavailable error (`-32002`); capabilities
use `sealed_head: null`. `pending` is unsupported, not an alias for sealed history.

A numeric block request above the served head, or a logs range crossing it, fails with `-32005`:
`the-legacy: block above sealed head <n>; configure an upstream or retry after the next seal`.
No silent truncation is allowed. Include structured error data such as
`{"reason":"above_sealed_head","sealed_head":20086783}` so limit errors using the same JSON-RPC
code remain distinguishable. Historical state, mempool, submission, fee methods and pending
requests return `-32004` with a method-specific unsupported explanation. Missing hashes use each
method's ordinary not-found semantics; an unknown hash does not reveal a block height.

With an upstream, Solo keeps the splitting-proxy role in RFC-0001. The storage routing boundary
is **available sealed coverage**, not merely upstream finality: a finalized block in a relic not
yet sealed or downloaded still goes upstream. Crossing logs ranges are split without gaps or
duplicates. Live tags continue to refer to the upstream view in this mode.

`legacy_capabilities` takes no parameters and returns, for example:

```json
{
  "chain_id": 1,
  "spec_version": 1,
  "sealed_head": 20086783,
  "head_pact_root": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "mode": "sealed-only",
  "limits": {
    "getlogs_max_blocks": 100000,
    "getlogs_max_results": 100000,
    "max_scanned_bytes_per_req": 2147483648,
    "batch_max": 1000
  },
  "tables": ["headers", "transactions", "receipts", "logs", "withdrawals"],
  "sidecars": ["logs.addr", "logs.topics", "txhash", "blockhash", "tx.to", "tx.from"],
  "cleaning": {
    "manifest_structure": true,
    "relic_linkage": true,
    "pact_chain": true,
    "file_hashes": false,
    "transactions_root": false,
    "receipts_root": false,
    "withdrawals_root": false,
    "checkpoint_anchor": false
  },
  "mirrors": [{"name": "example", "base_url": "https://legacy.example", "transport": "https"}]
}
```

The all-zero root is an example placeholder, not a valid claim about a published corpus. The
cleaning keys match `solo clean`; true means completed for all applicable data admitted under
this snapshot, never merely configured or scheduled. False includes not checked. Traces are
reported separately as unverified and are never covered by the trie/anchor flags. Capabilities
are the mirror's report, not independent evidence of correctness.

Tables and sidecars list coverage guaranteed across the applicable served range. Partial sidecar
availability needs per-relic discovery; it must not become a global true claim. Empty withdrawals
before activation do not mean a missing required table. Capability limits reflect runtime config.
HTTP JSON-RPC responses carry `X-Legacy-Sealed-Head: <n>` for their snapshot; omit it if there is
no head. The consumer can choose window sizes without repeated provider-limit probes.

**Acceptance:** an unmodified ponder or rindexer in a documented event-only configuration reaches
the mirror's head with Solo as its sole RPC URL. Record the exact client version/configuration;
head polling may continue after it catches up. Test empty startup, snapshot refresh, crossing
ranges, pending, and batch consistency. Capabilities round-trip through serde and reflect both
config and actual verification state.

### P2. `legacy-reader`: the native path

Introduce `crates/legacy-reader`, depending on `legacy-format`, `legacy-parquet`, `object_store`,
`roaring` and appropriate alloy types. It knows nothing about JSON-RPC. The Parquet codec remains
shared rather than being implemented separately in Solo and the reader.

```rust,ignore
let corpus = Corpus::open(registry_url, CleanPolicy::Hashes).await?;
corpus.sealed_head(); // Option<u64>
corpus.logs(&filter, from..=to);
corpus.blocks(from..=to, Fields::HEADERS | Fields::TRANSACTIONS);
corpus.receipts(from..=to);
corpus.active_blocks(address, from..=to, ActiveIn::LOGS | ActiveIn::TX_TO);
corpus.relic(index).manifest();
```

Queries stream results; active blocks can be returned as a `RoaringTreemap`. These names are a
sketch, not an API already present. Policies are cumulative and enforced before yielding rows:
`Structure` checks manifests/linkage/pact; `Hashes` adds full-file BLAKE3; `Tries` adds transactions,
receipts and withdrawals roots plus reconstructed header hashes/linkage; `Anchored` adds trusted
checkpoint linkage. Unimplemented policies are absent from the public enum, never accepted and
ignored. Errors identify the failed check and required policy.

Full-file BLAKE3 cannot authenticate arbitrary cold byte ranges from a manifest digest alone.
`Hashes` therefore downloads and verifies the complete relevant file before its first row is
yielded, then reuses an immutable cache keyed by the expected hash. `Tries` needs the full set of
dependent tables. Neither policy authenticates a producer to a chain without an anchor. Sidecars
that can suppress candidate rows must be validated/rebuilt from admitted tables before use as
exact exclusion indexes; a hash of a malicious producer's own sidecar is insufficient.

Solo becomes a JSON-RPC adapter over this reader, with one logs query implementation shared by
native consumers. An optional `LegacyProvider` adapter may live separately to keep dependencies
small. Unsupported state and above-head reads remain explicit errors.

**Acceptance:** Nuthatch backfills an event nest without a paid or upstream RPC configured and
matches the normalized ordered events from an archive-source fixture. Byte identity of derived
Parquet is tested only with the same pinned writer/configuration; it is not a cross-version
correctness requirement. Solo's RPC result parity is unchanged. Measure binary size and explain
any regression rather than assuming an extraction of shared code guarantees identical size.

### P3. Transaction address sidecars and `legacy_activeBlocks`

Add `idx/tx.to.roaring` and `idx/tx.from.roaring`, mapping addresses to transaction rows and row
groups, using portable Roaring serialization. Contract creation has null `to`; a future
`receipts.created.roaring` may index creation addresses separately.

The existing row and row-group bitmaps do **not** tell a reader exact block numbers without a
row-to-block mapping. To answer without data pages, the new sidecar profile must also include
an exact bitmap of block offsets (`block_number - relic.start`, in `0..8192`) per address. An
equivalent versioned extension is needed for log-address sidecars. Results union the requested
activity classes, intersect the block range, deduplicate, then add the relic start. Row-group
membership alone must never be reported as exact activity.

The container needs a byte-precise specification before implementation: version/magic, key order,
directory lengths/offsets, bitmap serialization choices, source-table hash binding and bounds
checks. RFC-0001 §7.1 currently describes the structure, not all these bytes. Output must be
deterministic and rebuildable from the corresponding Parquet table alone.

The current `FileEntry.table` enum accepts only data tables, so these files cannot simply be
appended to `files`. Propose an optional `index_files` manifest array with name, byte size, BLAKE3
and source-table content-hash binding, adopted with format validation and a spec-version decision.
For newly sealed relics it is covered by the manifest hash. An already sealed manifest is never
rewritten to add an index: a mirror may build a local cache bound to the existing table hash, or
scan when the index is absent. Local caches do not acquire a pact commitment by being adjacent
to a relic. This is an explicit amendment, not a claim that the present manifest already fits.

`legacy_activeBlocks` accepts one object:

```json
{"address":"0x0000000000000000000000000000000000000001","fromBlock":"0x0","toBlock":"0x1fff","in":["logs","to","from"],"format":"ranges"}
```

Default output is a sorted array of hex-quantity block numbers. `ranges` returns sorted maximal
inclusive `[start,end]` pairs, also hex quantities. Require a nonempty `in` list with recognized
values. Restrict the range with `getlogs_max_blocks`, scan and response budgets; never truncate.
Use exact sidecars when available, otherwise scan only the needed columns. Capabilities must not
promise a data-page-free answer where a fallback is required. A 50M-block query at a 100,000-block
limit is 500 requests, not one.

At 40,000 selected blocks, 20 CU per follow-up call and the §2.1 rate, provider compute is $0.42
instead of $525. That saving applies only if the predicate is complete for the consumer's task.
Top-level sender/recipient and log-emitter matches miss internal-only interactions and many state
changes. Never use them as an exhaustive trace/state filter without that qualification.

**Acceptance:** results across windows covering 1M blocks equal a direct distinct-block scan of
the same tables, including empty ranges, duplicates across activity classes and creation cases.
Benchmark against RFC-0001 §15's target under equivalent cache and range limits. Independent
rebuilds produce identical sidecars. Measure Base size overhead before setting defaults; the
suggested low-single-digit percentage is currently an unverified hypothesis.

### P4. Archive-RPC Shadow as a cost-bounded producer

Promote it to first-class for silos without a local history source, and a bootstrap option for
silos with one. A history-capable provider is sufficient; archive state access is not intrinsically
needed for bodies and receipts.

Fetch each block with `eth_getBlockByNumber(n, true)` and `eth_getBlockReceipts(n)`. Derive logs
from receipts. No per-transaction receipt fallback is implicit: a provider lacking block receipts
must fail capability validation or require an explicitly budgeted alternative. The block response
does not necessarily contain raw signed bytes; reconstruct supported transaction envelopes from
fields and verify their hashes, rather than assuming an RPC source inherently supplies them.

Profiles describe supported methods, historical coverage, batch limits, billing units and
throughput limits. Discovery probes are requests too and count against the budget. Offline
profiles and a plan-only command must permit estimation without contacting any endpoint. Batching
can reduce HTTP overhead; do not assume it reduces billable method calls or guarantees lower
latency. Provider-specific billing of batched members must be verified separately.

Stage one relic at a time. Persist validated block responses and a resumable journal; after a
crash resume missing work without discarding a whole paid partial relic. A response lost between
receipt and durable journalling may require a repeat, which is charged to the remaining budget.
Seal only complete, finalized ranges and publish the registry last. Validate chain identity,
contiguous block hashes and receipt/transaction association before sealing; empty or partial RPC
responses are not proof of an empty block.

`--budget-cu` caps estimated billing CU and `--budget-requests` caps logical RPC method attempts,
including probes, retries and batch members. Count HTTP exchanges separately. Reserve budget
before dispatch, including concurrent in-flight requests. Prefer stopping before starting a relic
that cannot fit; if retries consume the remaining reservation, stop with a durable partial relic,
not an overspend to reach a boundary. Never call an unknown-cost method under a CU budget without
a conservative configured bound. Reports distinguish attempted calls, estimated CU, completed
relics and staged work from an actual provider invoice.

Keep provider provenance mirror-local, outside manifests, in a `provenance.json` that records
provider aliases and redacted discovery results. Never retain API keys, credential-bearing URLs
or headers. This avoids coupling published corpus metadata to infrastructure and secrets. It does
**not** guarantee equal pact roots across independent producers: different file bytes and producer
metadata already change those roots. Content hashes are the cross-producer comparison.

**Illustrative seed costs:** two 20-CU methods per block, the §2.1 Alchemy rate, and dRPC's
[published flat method rate](https://blog.drpc.org/announcing-flat-pricing-simple-transparent-fair/)
of $6 per million requests. Treat each method as billable; do not discount a JSON-RPC batch.
These input block counts are scenarios supplied with the proposal, not verified current heights.

| Scenario | Blocks read via RPC | Alchemy compute | dRPC methods |
|---|---:|---:|---:|
| Mainnet pre-merge via downloaded era1 | 0 | $0 | $0 |
| Mainnet post-merge illustration | 10.5M | $220.50 | $126 |
| Base illustration | 52M | $1,092 | $624 |
| Arbitrum illustration | 500M | $10,500 | $6,000 |

These exclude finality discovery, retries, throughput add-ons, data transfer, storage, local CPU
and node costs. A static-file source avoids RPC charges, not all expenditure. Arbitrum's example
strongly favours evaluating a node/file source before paid per-block ingestion; its chain-specific
encoding remains unverified under RFC-0001 §12.

**Acceptance:** first use deterministic local HTTP fixtures to test budgets, unavailable methods,
out-of-order batch replies, errors, finality and crash/resume. A later explicitly authorized paid
Base run must stay within its configured bounds, pass `Hashes`, and match table content hashes
from an independent compatible producer. No paid endpoint is needed to implement or test the
format, cost accounting, or crash recovery. This RFC is not authorization to run a paid seed.

### P5. Roadmap re-sequencing

Nothing is removed from the original goals; Stage 1 is split:

- **1a - Served, honestly labelled.** Complete the applicable table codecs and query indexes;
  add era1 ingestion and sealed-only Solo with capabilities. Publish complete pre-merge mainnet
  relics. The final partial era1 at the merge cannot by itself seal an 8192-block relic; leave that
  boundary relic unpublished until the missing post-merge portion is available. Withdrawals are
  not applicable to this pre-merge corpus. Acceptance is P1 against the actual published head,
  not a promise of coverage precisely through the merge.
- **1b - Paid seed and native consumption.** Add the cost-bounded RPC producer, post-merge mainnet
  and Base production when separately authorized, `legacy-reader` with `Hashes`, shared Solo query
  execution, and P3's transaction sidecars. Acceptance is P2/P3 plus a published Base registry.
- **1c - Anchored verification.** Add trie rebuilding, reconstructed header checks, checkpoint
  anchoring, and the `Tries`/`Anchored` policies; add Reth static-file ingestion. Acceptance is the
  corrected RFC-0001 Stage 1 requirement: independent content agreement and exact-mirror pact
  agreement, with end-to-end cleaning.

Stages 2 and 3 retain their goals. ExEx requires a suitable node. P2 is designed into the codec
boundary now even if the full reader crate lands in 1b, to avoid duplicating the Solo query engine.

**Serving below `Hashes`:** proposed only as an explicit exception using
`[verify] serve_below_policy = true` (default false), a non-suppressible startup line naming checks
actually completed, matching capabilities/MCP reporting, and no claim of verified chain history.
Structural consistency alone does not establish even file integrity, let alone equivalence to a
trusted RPC provider. Whether 1a should instead require `Hashes` from its first release remains
open. The writer already calculates file hashes, but a mirror must independently check them.

### P6. Porter: the unsealed-range sibling

Porter gets its own RFC-0003; neither that RFC nor a Porter implementation is created by this
proposal. One service per chain shares upstream head/finality polling and coalesces compatible
log filters across clients. It remains temporary cached state, never a sealed-relic producer.

For coalescing, union addresses only if every filter constrains addresses; a wildcard address
means wildcard in the upstream superset. At each topic position, a wildcard in any participating
filter makes that position unconstrained. Per-position unions form a superset of client filters,
so responses must be filtered again per client. Group overly broad combinations when response
caps or scanned-work budgets would erase the benefit. Per-client cursors and delivery state make
this a cache/proxy with state, not literally a stateless service.

Unsealed entries are keyed by block hash. On a parent mismatch, find the common ancestor,
invalidate all affected descendants and define removed-log/replay semantics in RFC-0003. Forward
the upstream's finality assertion; do not invent another consensus claim.

**Handoff:** route through one snapshot of Solo's actual served head. Solo's corpus owns through
that head; Porter/upstream covers the rest, including finalized blocks waiting for a complete
relic or publication. Split crossing ranges without omissions or duplicates. Evict handed-off
data only after the corresponding sealed range is published, readable and admitted by Solo, not
when `finalized >= relic.end`. If the mirror stalls, retained coverage or upstream fallback must
remain available. A regressed/conflicting corpus snapshot fails validation rather than silently
moving the boundary backwards.

Porter may route below-head requests to Solo, but must not independently treat them as mutable
tip data. This separates routing from ownership and avoids the contradictory requirement to both
forward and refuse the same range. Detailed Porter errors and deployment location remain open.

If N clients issue identical requests at compatible windows, upstream work can approach one
client's workload rather than N. The often quoted 67% at three clients and 95% at twenty are
ideal duplicate-work reductions, not guaranteed bill savings for dissimilar filters or cursors.

### P7. Operating a public mirror

Nuthatch proposes a public reference mirror per silo on R2, listed with other mirrors in
the registry. It has no privileged cryptographic status; trusted checkpoints remain separate.
Current [R2 Standard pricing](https://developers.cloudflare.com/r2/pricing/) lists $0.015/GB-month,
$0.36/million Class B reads and no egress charge. Hosting still incurs operations, storage and
serving costs, and access may be rate-limited. An open format does not promise unlimited free
service from every operator.

`solo mirror <registry-url>` would fetch and validate registry/manifests, resume downloads, clean
to the selected policy, and publish a local registry only after all advertised objects are ready.
Exact-copy mirrors retain the original manifests and pact; their registry may list new transport
locations. Atomic publication must survive interruption and remote object changes.

A producer node may be temporary; a sealed-only serving mirror needs none. Confirm hosting terms
for the actual workload before renting infrastructure, rather than inferring permission from an
unverified anecdote about a provider's node policy.

Nuthatch hosted event nests would backfill through the reader and follow the tip through Porter.
Rebuilding from cached relics plus config avoids paid history RPC only where that config needs
no unavailable state, traces or external data. Tip access and compute/storage costs remain.
Ecosystem grants are a possible funding source for one-time production, not a prerequisite or
an assumed award.

## 5. Interfaces

| Surface | Owner | Range / purpose |
|---|---|---|
| Supported history `eth_*` reads | Solo / reader | admitted sealed range |
| `legacy_capabilities` and sealed-head header | Solo | one admitted snapshot |
| `legacy_activeBlocks` | Solo / reader | bounded sealed range, explicit predicate |
| `legacy-reader` API | reader | native storage/query access |
| `tx.to`, `tx.from`, exact activity bitmaps | corpus / mirror cache | per relic |
| Proposed `index_files` | future manifest schema | committed sidecars for new relics |
| Redacted `provenance.json` | local operator | source/accounting context, not pact data |
| Head cache, coalescing, reorg handling | Porter, future RFC-0003 | above admitted sealed head |
| `solo mirror` | Solo | verified copy/publication workflow |

## 6. Non-goals

Historical state and unsealed data remain outside the corpus. Geometry and the pact algorithm
remain unchanged. Sidecar commitments do require the explicit schema amendment in P3, and sealed
manifests remain immutable. Trace authentication is not added. No hosted service, paid production
run or infrastructure deployment is authorized by adoption of this draft.

## 7. Consumer cost model

For a 50M-block single-address backfill, use the explicit method/window assumptions in §2.1.
Rates were read on 2026-09-26; workload sizes are illustrative. `ceil(50_000_000 / 8192) = 6104`
relics if starting at block zero, with the last partial range read from its containing sealed relic.

| Workload | Paid history provider | Public Solo | Native reader |
|---|---|---|---|
| Logs at 100,000-block windows | ~$0.016 compute | no provider RPC; operator pays hosting | object reads + local compute |
| Logs at 1,000-block windows | ~$1.575 compute | same | same |
| One 20-CU method per block | ~$525 compute | same | same |
| 40,000 selected follow-up blocks | ~$0.42 compute | sealed bodies from corpus | same |
| Re-index after adding a contract | repeat uncached history work | repeat scan, no provider RPC | local scan if already cached |

R2 marginal read-operation cost is `Class_B_requests * 0.36 / 1_000_000`, before monthly allowance
and billing rounding. For example, 6104 relics at four reads each is about $0.0088, or at twenty
reads each about $0.044. These are assumed request counts, not measured query costs. Cold
`Hashes` requires full files; bandwidth to the consumer, local storage, cache misses and decoding
may dominate even when R2 egress is free. On a public bucket, storage operations are billed to
the bucket operator, not automatically to the reader's cloud account.

The remaining paid dependencies are the unsealed range, historical state, and traces unavailable
in the corpus. There is no universal zero-cost claim for arbitrary indexer configurations.

## 8. Proposed amendments to RFC-0001

These are implementation-linked proposals, not claims that all changes land with this draft.
The canonical row encoding and exact-mirror versus content-identity corrections are already
specified in RFC-0001 and implemented for headers, transactions, receipts, logs and withdrawals. Local `solo clean --files`
now checks file integrity, those table codecs, header coverage and stored parent/boundary
consistency, transaction/receipt/log references and receipt/header blooms where the required tables exist, plus
RLP/Keccak header hash reconstruction for chain ID 1 using layouts through
Prague and execution gas accounting for chain ID 1 (RFC-0001 §10.7). Other chain profiles, consensus rules and checkpoint trust remain
unchecked. When a manifest carries `era1_accumulator_root` and headers were read, `solo clean --files`
recomputes that SSZ root from the stored block hashes and total difficulties; a missing root stays
unchecked, and the comparison does not reconstruct header RLP or apply a merge schedule. The native reader and its
policy API remain proposed. The following remain follow-ups.

| RFC-0001 | Required amendment |
|---|---|
| §3.2 | name Porter as a future sibling; preserve finalized-only immutable relics |
| §7 | define exact block-offset indexes and transaction-address containers per P3 |
| §8.3 | adopt validated sidecar commitments, provisionally `index_files`; decide spec version |
| §8.3, §11 | keep redacted source provenance local; use content agreement across producers |
| §11 source 1 | first-class RPC option with cost profiles, hard bounds and durable partial work |
| §13.1 | shared query/storage implementation in `legacy-reader` |
| §13.2 | use admitted sealed coverage for storage routing; add sealed-only mode |
| §13.3 | add capabilities/activity methods, snapshot tags and explicit unavailable/above-head errors |
| §13.7 | optional upstream and default-false below-Hashes serving exception |
| §17 | specify Nuthatch's native reader consumption path |
| §18 | split Stage 1 into 1a/1b/1c; retain the corrected verification acceptance |
| §19 | native access is a library over the corpus, not a new network protocol |

Each format/behaviour amendment must land with its corresponding implementation and checks.
Adding a roadmap proposal does not change what current executables accept or verify.

## 9. Whole-RFC acceptance

An unmodified third-party event indexer and Nuthatch via the reader backfill the same mainnet
contract from a public mirror and produce equal normalized event sets. The former uses Solo as
its RPC URL; neither uses a paid/upstream history provider. Capabilities accurately report the
mirror's checks. A fixture-backed network audit establishes that there are no history-provider
requests; if an operator separately authorizes a live trial, provider accounting corroborates it.
These integration tests are future acceptance criteria, not results of the current format tests.

## 10. Open questions

- Whether Stage 1a should require `Hashes` from day one rather than permit the explicit exception.
- The exact sidecar container, manifest evolution/version, local cache binding and bitmap limits.
- L2 sidecar defaults after actual size and selectivity measurements.
- `LegacyProvider` in `legacy-reader` or a separate `legacy-alloy` crate.
- Porter in this workspace or another repository; either must share the handoff semantics.
- Porter coalescing thresholds, subscription lifecycle, reorg replay and error codes in RFC-0003.
- JSON-RPC and MCP exposure for capabilities/activity; proposed primary interface is JSON-RPC.
- Contract-creation indexing via `receipts.created.roaring`.
- Standardized range-specific capability and cleaning coverage beyond the conservative global flags.

## 11. References

- [RFC-0001](0001-the-legacy.md), this repository.
- [Alchemy pricing](https://www.alchemy.com/pricing),
  [CU costs](https://www.alchemy.com/docs/reference/compute-unit-costs), and the
  [older pricing FAQ](https://www.alchemy.com/docs/reference/pay-as-you-go-pricing-faq).
- [Cloudflare R2 pricing](https://developers.cloudflare.com/r2/pricing/).
- [dRPC flat-rate announcement](https://blog.drpc.org/announcing-flat-pricing-simple-transparent-fair/)
  and [request pricing documentation](https://drpc.org/docs/pricing/requests).
- [Ethereum history endpoints](https://github.com/eth-clients/history-endpoints), candidate era1 sources.
- Nuthatch v3.11.0, PR #1501 and NW-RFC-002 are author-supplied motivation; their contents and
  measurements were not independently checked for this draft. No current Nuthatch behaviour is
  asserted as a result of this repository's tests.
