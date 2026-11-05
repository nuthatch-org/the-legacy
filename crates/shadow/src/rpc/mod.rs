//! The archive-RPC Shadow (RFC-0002 P4, RFC-0004 §7): plan, probe, fetch and journal, decode,
//! seal.
//!
//! Only validated responses reach the journal, and sealing reads the journal alone, so a crash
//! loses nothing already paid for and a re-seal needs no network. Everything after decoding is
//! the shared row, Parquet, manifest and pact code era1 uses.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use legacy_format::manifest::{Boundary, Manifest};
use legacy_format::relic::{self, BlockRange};
use legacy_format::{pact, Hash32};
use serde_json::{json, Value};
use thiserror::Error;

pub mod block;
pub mod client;
pub mod journal;
pub mod profile;
pub mod wire;

use client::{Budget, CallError, Client, METHOD_NOT_FOUND};
use journal::Journal;
use profile::Profile;
use wire::Journaled;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error("wire: {0}")]
    Wire(String),
    #[error("block {number}: {reason}")]
    Block { number: u64, reason: String },
    #[error("probe: {0}")]
    Probe(String),
    #[error(
        "request budget exhausted after {attempted} method attempts; {held} of {total} blocks are \
         journaled and a rerun with more budget resumes from there"
    )]
    Budget {
        attempted: u64,
        held: u64,
        total: u64,
    },
    #[error("journal: {0}")]
    Journal(String),
    #[error("{0}")]
    Call(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Format(#[from] legacy_format::Error),
    #[error(transparent)]
    Parquet(#[from] legacy_parquet::Error),
    #[error(transparent)]
    Seal(Box<dyn std::error::Error + Send + Sync>),
}

pub struct SealRequest {
    pub endpoint: String,
    /// Sent as `Authorization: Bearer`, for providers that keep the key out of the URL.
    pub bearer: Option<String>,
    pub profile: Profile,
    pub journal: PathBuf,
    pub out: PathBuf,
    pub from: u64,
    pub to: u64,
    pub chain_id: u64,
    pub silo: String,
    pub predecessor: Option<PathBuf>,
    /// Seal with `detached_pact_start` instead of a predecessor (RFC-0001 §8.4.1).
    pub detached: bool,
    pub budget_requests: Option<u64>,
    pub budget_cu: Option<u64>,
    pub concurrency: usize,
    pub retries: u32,
    /// Fetch and journal only: no `--after`, no `--out`, nothing sealed.
    pub journal_only: bool,
    /// Total difficulty of the block before the relic, as a minimal big-endian magnitude, for a
    /// provider that serves no pre-merge `totalDifficulty`. Not header-committed; recorded.
    pub total_difficulty_base: Option<Vec<u8>>,
}

/// What one run did, sealed or not.
#[derive(Debug)]
pub struct Run {
    pub relic_index: u64,
    pub held_before: u64,
    pub held_after: u64,
    pub fetched: u64,
    pub attempted: u64,
    pub http_exchanges: u64,
    pub estimated_cu: Option<u64>,
}

#[derive(Debug)]
pub struct Sealed {
    pub relic_index: u64,
    pub start: u64,
    pub end: u64,
    pub chain_id: u64,
    pub pact_root: Hash32,
    pub directory: PathBuf,
    pub tables: Vec<String>,
    pub transactions: u64,
    pub withdrawals: Option<u64>,
    /// Pre-merge total difficulty was derived from `--total-difficulty-base`, not served.
    pub derived_total_difficulty: bool,
    pub fetched: u64,
    pub held_before: u64,
    pub attempted: u64,
    pub http_exchanges: u64,
    pub estimated_cu: Option<u64>,
}

/// The relic a request names, refusing anything but one whole aligned relic.
pub fn relic_of(from: u64, to: u64) -> Result<(u64, BlockRange), Error> {
    let index = relic::relic_index(from);
    let range = relic::relic_range(index);
    if from != range.start || to != range.end {
        return Err(Error::Message(format!(
            "blocks {from}..={to} are not one relic; relic {index} is {}..={}",
            range.start, range.end
        )));
    }
    Ok((index, range))
}

pub fn seal(request: &SealRequest) -> Result<Sealed, Error> {
    let (run, sealed) = run(request)?;
    let mut sealed =
        sealed.ok_or_else(|| Error::Message("journal-only run was asked to seal".into()))?;
    sealed.fetched = run.fetched;
    sealed.held_before = run.held_before;
    sealed.attempted = run.attempted;
    sealed.http_exchanges = run.http_exchanges;
    sealed.estimated_cu = run.estimated_cu;
    Ok(sealed)
}

/// Fetch and journal one relic without sealing it.
pub fn journal_only(request: &SealRequest) -> Result<Run, Error> {
    Ok(run(request)?.0)
}

fn run(request: &SealRequest) -> Result<(Run, Option<Sealed>), Error> {
    let (index, range) = relic_of(request.from, request.to)?;
    if request.profile.provider.chain_id != request.chain_id {
        return Err(Error::Message(format!(
            "profile is for chain {}, the seal is for chain {}",
            request.profile.provider.chain_id, request.chain_id
        )));
    }
    if !request.profile.methods.get_block_receipts {
        return Err(Error::Probe(
            "profile says eth_getBlockReceipts is unavailable; per-transaction receipts are not a \
             silent fallback and need their own budgeted run"
                .into(),
        ));
    }
    if request.budget_cu.is_some() && request.profile.billing.cu_per_method.is_none() {
        return Err(Error::Message(
            "--budget-cu needs billing.cu_per_method in the profile; a cap over an unknown price \
             bounds nothing"
                .into(),
        ));
    }
    // Everything that can be refused for free is refused before the first request.
    let predecessor = match &request.predecessor {
        _ if request.journal_only => None,
        Some(path) => {
            Some(crate::era1::load_manifest(path).map_err(|e| Error::Message(e.to_string()))?)
        }
        None if index == 0 => None,
        None if request.detached => None,
        None => {
            return Err(Error::Message(format!(
                "relic {index} is not the genesis relic; pass --after with the predecessor manifest, or --detached"
            )))
        }
    };
    if let Some(prev) = &predecessor {
        if prev.relic_index() + 1 != index {
            return Err(Error::Message(format!(
                "--after is relic {}, not relic {}",
                prev.relic_index(),
                index - 1
            )));
        }
    }
    if !request.journal_only {
        crate::era1::prepare_output(&request.out).map_err(|e| Error::Message(e.to_string()))?;
    }

    let journal = Journal::open(&request.journal)?;
    let missing = journal.missing(range)?;
    let total = relic::BLOCKS_PER_RELIC;
    let held_before = total - missing.len() as u64;
    let budget = Budget::new(
        request.budget_requests,
        request.budget_cu,
        request.profile.billing.cu_per_method,
    );
    let needed = Profile::requests_for(missing.len() as u64);
    if !budget.fits(needed) {
        let (requests, cu) = budget.limits();
        return Err(Error::Message(format!(
            "relic {index} needs at least {needed} method calls for {} missing blocks (before \
             retries); the budget is {} requests / {} CU, so nothing was fetched",
            missing.len(),
            requests.map_or("unbounded".into(), |n| n.to_string()),
            cu.map_or("unbounded".into(), |n| n.to_string()),
        )));
    }

    let client = Client::new(
        request.endpoint.clone(),
        request.bearer.clone(),
        &request.profile,
        budget,
    );
    let progress = Progress::new(&journal, index, held_before);
    let probed = probe(&client, request.chain_id, range);
    let fetched = match &probed {
        Ok(_) => fetch(
            &client,
            &journal,
            &request.profile,
            missing,
            request,
            &progress,
        ),
        Err(_) => Ok(0),
    };
    let result = match (&probed, &fetched) {
        (Ok(_), Ok(_)) if !request.journal_only => {
            assemble(&journal, index, range, request, predecessor.as_ref()).map(Some)
        }
        _ => Ok(None),
    };
    let held_after = journal.held(range)?.len() as u64;
    let budget = &client.budget;
    let state = match (&probed, &fetched, &result) {
        (Err(_), _, _) => "probe failed",
        (_, Err(Error::Budget { .. }), _) => "budget exhausted",
        (_, Err(_), _) => "failed",
        (_, _, Err(_)) => "seal failed",
        (_, _, Ok(Some(_))) => "sealed",
        _ => "journaled",
    };
    let failure = [
        probed.as_ref().err(),
        fetched.as_ref().err(),
        result.as_ref().err(),
    ]
    .into_iter()
    .flatten()
    .next()
    .map(|e| e.to_string());
    progress.finish(&client, held_after, state, failure.as_deref());

    let mut runs = std::fs::read(journal.root().join("provenance.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("runs").cloned())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    runs.push(json!({
        "started_unix": progress.started,
        "state": state,
        "failure": failure,
        "probes": probed.as_ref().ok(),
        "fetched_blocks": fetched.as_ref().ok(),
        "held_before": held_before,
        "held_after": held_after,
        "method_attempts": budget.attempted(),
        "http_exchanges": budget.http_exchanges(),
        "estimated_cu": budget.estimated_cu(),
        "budget_requests": request.budget_requests,
        "budget_cu": request.budget_cu,
        "total_difficulty_base": request.total_difficulty_base.as_deref().map(hex::encode),
    }));
    journal.write_provenance(&json!({
        "provider_alias": request.profile.provider.alias,
        "chain_id": request.chain_id,
        "relic": index,
        "block_range": {"start": range.start, "end": range.end},
        "shadow_checks": SHADOW_CHECKS,
        "runs": runs,
    }))?;
    probed?;
    let fetched = fetched?;
    let sealed = result?;
    Ok((
        Run {
            relic_index: index,
            held_before,
            held_after,
            fetched,
            attempted: budget.attempted(),
            http_exchanges: budget.http_exchanges(),
            estimated_cu: budget.estimated_cu(),
        },
        sealed,
    ))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `progress.json` beside the journal, rewritten every few seconds so a dashboard can follow a
/// run without touching the Shadow. Mirror-local, like the rest of the journal directory.
struct Progress {
    path: PathBuf,
    relic: u64,
    held_before: u64,
    started: u64,
    transient: AtomicU64,
    rejected: AtomicU64,
    last_rejection: Mutex<Option<String>>,
}

impl Progress {
    fn new(journal: &Journal, relic: u64, held_before: u64) -> Self {
        Self {
            path: journal.root().join("progress.json"),
            relic,
            held_before,
            started: unix_now(),
            transient: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            last_rejection: Mutex::new(None),
        }
    }

    fn write(&self, client: &Client, held: u64, state: &str, failure: Option<&str>) {
        let value = json!({
            "relic": self.relic,
            "block_range": {"start": relic::relic_range(self.relic).start, "end": relic::relic_range(self.relic).end},
            "state": state,
            "failure": failure,
            "held": held,
            "held_before": self.held_before,
            "total": relic::BLOCKS_PER_RELIC,
            "method_attempts": client.budget.attempted(),
            "http_exchanges": client.budget.http_exchanges(),
            "budget_requests": client.budget.limits().0,
            "transient_errors": self.transient.load(Ordering::SeqCst),
            "rejected_responses": self.rejected.load(Ordering::SeqCst),
            "last_rejection": *self.last_rejection.lock().expect("progress lock"),
            "started_unix": self.started,
            "updated_unix": unix_now(),
        });
        let temp = self.path.with_extension("json.tmp");
        // A dashboard aid, so a failed write is not worth stopping a paid run for.
        if std::fs::write(&temp, value.to_string()).is_ok() {
            let _ = std::fs::rename(&temp, &self.path);
        }
    }

    fn finish(&self, client: &Client, held: u64, state: &str, failure: Option<&str>) {
        self.write(client, held, state, failure);
    }
}

/// What the Shadow checked on its input at fetch time, which is not a cleaning claim about the
/// published files (RFC-0004 §9.2).
pub const SHADOW_CHECKS: &str =
    "served block hash equals the rebuilt header hash; every transaction \
envelope rebuilt from fields hashes to its served hash; recovered sender equals served `from`; \
served gasUsed, contractAddress, effectiveGasPrice and logIndex equal the derived values; \
transaction, receipt and withdrawal roots, blooms and gas accounting per block";

fn probe(client: &Client, chain_id: u64, range: BlockRange) -> Result<Value, Error> {
    let served = client
        .call("eth_chainId", json!([]))
        .map_err(|e| Error::Probe(format!("eth_chainId: {e}")))?;
    let served = served
        .as_str()
        .map(|t| wire::u64_of("chainId", t))
        .transpose()?
        .ok_or_else(|| Error::Probe("eth_chainId did not return a quantity".into()))?;
    if served != chain_id {
        return Err(Error::Probe(format!(
            "endpoint serves chain {served}, not {chain_id}"
        )));
    }
    let finalized = client
        .call("eth_getBlockByNumber", json!(["finalized", false]))
        .map_err(|e| Error::Probe(format!("finalized block: {e}")))?;
    let finalized = finalized
        .get("number")
        .and_then(Value::as_str)
        .map(|t| wire::u64_of("number", t))
        .transpose()?
        .ok_or_else(|| Error::Probe("finalized block has no number".into()))?;
    if finalized < range.end {
        return Err(Error::Probe(format!(
            "finalized head is {finalized}, before relic end {}; a relic is sealed only past finality",
            range.end
        )));
    }
    Ok(json!({"chain_id": served, "finalized": finalized}))
}

enum Outcome {
    Done,
    Retry(String),
    /// The provider answered and the answer failed validation.
    Invalid(String),
    Fatal(Error),
    Budget,
}

fn fetch_blocks(
    client: &Client,
    numbers: &[u64],
    batch: bool,
) -> Vec<(u64, Outcome, Option<Journaled>)> {
    let mut replies: Vec<(Result<Value, CallError>, Result<Value, CallError>)> = Vec::new();
    if batch {
        let calls: Vec<(&str, Value)> = numbers
            .iter()
            .flat_map(|n| {
                let tag = wire::hex_u64(*n);
                [
                    ("eth_getBlockByNumber", json!([tag, true])),
                    ("eth_getBlockReceipts", json!([tag])),
                ]
            })
            .collect();
        match client.batch(&calls) {
            Ok(members) => {
                let mut members = members.into_iter();
                for _ in numbers {
                    let block = members.next().expect("one block member per number");
                    let receipts = members.next().expect("one receipts member per number");
                    replies.push((block, receipts));
                }
            }
            Err(e) => {
                for _ in numbers {
                    replies.push((Err(e.clone()), Err(e.clone())));
                }
            }
        }
    } else {
        for n in numbers {
            let tag = wire::hex_u64(*n);
            let block = client.call("eth_getBlockByNumber", json!([tag, true]));
            let receipts = match &block {
                Err(CallError::Budget) => Err(CallError::Budget),
                _ => client.call("eth_getBlockReceipts", json!([tag])),
            };
            replies.push((block, receipts));
        }
    }
    numbers
        .iter()
        .zip(replies)
        .map(|(n, (block, receipts))| {
            let outcome = match (block, receipts) {
                (Err(CallError::Budget), _) | (_, Err(CallError::Budget)) => Outcome::Budget,
                (_, Err(CallError::Rpc { code, message })) if code == METHOD_NOT_FOUND => {
                    Outcome::Fatal(Error::Probe(format!(
                        "eth_getBlockReceipts is unsupported ({message}); per-transaction \
                         receipts are not a silent fallback"
                    )))
                }
                (Err(CallError::Rpc { code, message }), _)
                | (_, Err(CallError::Rpc { code, message })) => Outcome::Fatal(Error::Call(
                    format!("block {n}: JSON-RPC error {code}: {message}"),
                )),
                (Err(e), _) | (_, Err(e)) => Outcome::Retry(e.to_string()),
                (Ok(Value::Null), _) | (_, Ok(Value::Null)) => {
                    // Null is not proof of absence: the provider may not hold this depth.
                    Outcome::Retry("provider returned null".into())
                }
                (Ok(block), Ok(receipts)) => {
                    match (
                        serde_json::from_value(block),
                        serde_json::from_value(receipts),
                    ) {
                        (Ok(block), Ok(receipts)) => {
                            let journaled = Journaled { block, receipts };
                            match block::rows(*n, &journaled) {
                                Ok(_) => return (*n, Outcome::Done, Some(journaled)),
                                Err(e @ Error::Probe(_)) => Outcome::Fatal(e),
                                Err(e) => Outcome::Invalid(e.to_string()),
                            }
                        }
                        (Err(e), _) | (_, Err(e)) => Outcome::Invalid(format!("unparseable: {e}")),
                    }
                }
            };
            (*n, outcome, None)
        })
        .collect()
}

fn fetch(
    client: &Client,
    journal: &Journal,
    profile: &Profile,
    missing: Vec<u64>,
    request: &SealRequest,
    progress: &Progress,
) -> Result<u64, Error> {
    let Some(&first) = missing.first() else {
        return Ok(0);
    };
    let per_batch = profile.methods.batch_max.max(1);
    let batch = profile.methods.batch_max > 0;
    let range = relic::relic_range(relic::relic_index(first));
    let queue: Mutex<VecDeque<(u64, u32)>> =
        Mutex::new(missing.iter().skip(1).map(|n| (*n, 0)).collect());
    // Blocks neither journaled nor abandoned. A retry stays outstanding while it waits in the queue.
    let outstanding = AtomicU64::new(missing.len() as u64);
    let stop = AtomicBool::new(false);
    let exhausted = AtomicBool::new(false);
    let failure: Mutex<Option<Error>> = Mutex::new(None);
    let fetched = AtomicU64::new(0);

    let handle = |work: &[(u64, u32)], batch: bool| {
        let numbers: Vec<u64> = work.iter().map(|(n, _)| *n).collect();
        for ((n, outcome, journaled), (_, attempts)) in
            fetch_blocks(client, &numbers, batch).into_iter().zip(work)
        {
            let fatal = match outcome {
                Outcome::Done => {
                    match journal.put(n, &journaled.expect("done carries its block")) {
                        Ok(()) => {
                            fetched.fetch_add(1, Ordering::SeqCst);
                            outstanding.fetch_sub(1, Ordering::SeqCst);
                            None
                        }
                        Err(e) => Some(e),
                    }
                }
                Outcome::Budget => {
                    exhausted.store(true, Ordering::SeqCst);
                    stop.store(true, Ordering::SeqCst);
                    None
                }
                Outcome::Fatal(e) => Some(e),
                Outcome::Invalid(why) | Outcome::Retry(why) if *attempts >= request.retries => {
                    Some(Error::Block {
                        number: n,
                        reason: format!(
                            "still failing after {} attempts, not journaled: {why}",
                            attempts + 1
                        ),
                    })
                }
                Outcome::Invalid(why) => {
                    progress.rejected.fetch_add(1, Ordering::SeqCst);
                    *progress.last_rejection.lock().expect("progress lock") =
                        Some(format!("block {n}: {why}"));
                    queue
                        .lock()
                        .expect("queue lock")
                        .push_back((n, attempts + 1));
                    None
                }
                Outcome::Retry(_) => {
                    progress.transient.fetch_add(1, Ordering::SeqCst);
                    queue
                        .lock()
                        .expect("queue lock")
                        .push_back((n, attempts + 1));
                    None
                }
            };
            if let Some(error) = fatal {
                failure.lock().expect("failure lock").get_or_insert(error);
                stop.store(true, Ordering::SeqCst);
            }
        }
    };

    // The first block goes alone: it doubles as the receipts probe, so an unsupported method stops
    // the run after two calls rather than after a full wave of concurrent ones.
    handle(&[(first, 0)], false);

    let held_before = relic::BLOCKS_PER_RELIC - missing.len() as u64;
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::SeqCst) && outstanding.load(Ordering::SeqCst) > 0 {
                let held = held_before + fetched.load(Ordering::SeqCst);
                progress.write(client, held, "fetching", None);
                for _ in 0..20 {
                    if stop.load(Ordering::SeqCst) || outstanding.load(Ordering::SeqCst) == 0 {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        });
        for _ in 0..request.concurrency.max(1) {
            scope.spawn(|| {
                while !stop.load(Ordering::SeqCst) && outstanding.load(Ordering::SeqCst) > 0 {
                    let work: Vec<(u64, u32)> = {
                        let mut queue = queue.lock().expect("queue lock");
                        let take = per_batch.min(queue.len());
                        queue.drain(..take).collect()
                    };
                    if work.is_empty() {
                        // Another worker holds the rest and may yet requeue a retry.
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    handle(&work, batch);
                }
            });
        }
    });

    if let Some(error) = failure.into_inner().expect("failure lock") {
        return Err(error);
    }
    if exhausted.load(Ordering::SeqCst) {
        return Err(Error::Budget {
            attempted: client.budget.attempted(),
            held: journal.held(range)?.len() as u64,
            total: relic::BLOCKS_PER_RELIC,
        });
    }
    Ok(fetched.into_inner())
}

fn assemble(
    journal: &Journal,
    index: u64,
    range: BlockRange,
    request: &SealRequest,
    predecessor: Option<&Manifest>,
) -> Result<Sealed, Error> {
    let missing = journal.missing(range)?;
    if !missing.is_empty() {
        return Err(Error::Journal(format!(
            "{} blocks still missing, first {}",
            missing.len(),
            missing[0]
        )));
    }
    let mut headers = Vec::with_capacity(relic::BLOCKS_PER_RELIC as usize);
    let mut transactions = Vec::new();
    let mut receipts = Vec::new();
    let mut logs = Vec::new();
    let mut withdrawals = Vec::new();
    let mut has_withdrawals = false;
    for number in range.start..=range.end {
        let rows = block::rows(number, &journal.get(number)?)?;
        headers.push(rows.header);
        transactions.extend(rows.transactions);
        receipts.extend(rows.receipts);
        logs.extend(rows.logs);
        if let Some(list) = rows.withdrawals {
            has_withdrawals = true;
            withdrawals.extend(list);
        }
    }
    let derived_td =
        block::fill_total_difficulty(&mut headers, request.total_difficulty_base.as_deref())?;
    for pair in headers.windows(2) {
        if pair[1].parent_hash != pair[0].block_hash {
            return Err(Error::Block {
                number: pair[1].block_number,
                reason: "parent hash does not link to the journaled previous block; delete both \
                         journal files and resume"
                    .into(),
            });
        }
    }
    let boundary = Boundary {
        start_block_hash: Hash32::new(headers[0].block_hash),
        end_block_hash: Hash32::new(headers[headers.len() - 1].block_hash),
        parent_hash_of_start: Hash32::new(headers[0].parent_hash),
    };
    let boxed = |e: Box<dyn std::error::Error + Send + Sync>| Error::Seal(e);
    legacy_format::headers::verify_relic_rows(&headers, range, boundary)
        .map_err(|e| boxed(e.into()))?;
    legacy_format::ethereum_transactions::verify_transaction_roots(&headers, &transactions)
        .map_err(|e| boxed(e.into()))?;
    legacy_format::ethereum_receipts::verify_receipt_roots(&headers, &receipts, &logs)
        .map_err(|e| boxed(e.into()))?;
    legacy_format::bloom::receipt_blooms(&logs, &receipts).map_err(|e| boxed(e.into()))?;
    legacy_format::bloom::header_blooms(&headers, &receipts).map_err(|e| boxed(e.into()))?;
    legacy_format::consistency::ethereum_receipt_gas(&headers, &receipts)
        .map_err(|e| boxed(e.into()))?;
    if has_withdrawals {
        legacy_format::ethereum_withdrawals::verify_withdrawal_roots(&headers, &withdrawals)
            .map_err(|e| boxed(e.into()))?;
    }

    let mut written = vec![
        legacy_parquet::headers::write_headers(&headers)?,
        legacy_parquet::transactions::write_transactions(&transactions)?,
        legacy_parquet::receipts::write_receipts(&receipts)?,
        legacy_parquet::logs::write_logs(&logs)?,
    ];
    if has_withdrawals {
        written.push(legacy_parquet::withdrawals::write_withdrawals(
            &withdrawals,
        )?);
    }
    for (bytes, entry) in &written {
        std::fs::write(request.out.join(&entry.name), bytes)?;
    }
    let mut manifest = Manifest::new(request.chain_id, &request.silo, index, boundary);
    manifest.detached_pact_start = request.detached && predecessor.is_none() && index != 0;
    manifest.files = written.iter().map(|(_, entry)| entry.clone()).collect();
    let pact_root = pact::seal_chain(predecessor, std::slice::from_mut(&mut manifest))?;
    std::fs::write(
        request.out.join("manifest.json"),
        manifest.to_canonical_bytes()?,
    )?;
    Ok(Sealed {
        relic_index: index,
        start: range.start,
        end: range.end,
        chain_id: request.chain_id,
        pact_root,
        directory: request.out.clone(),
        tables: written
            .iter()
            .map(|(_, e)| e.table.file_name().trim_end_matches(".parquet").to_string())
            .collect(),
        transactions: transactions.len() as u64,
        withdrawals: has_withdrawals.then_some(withdrawals.len() as u64),
        derived_total_difficulty: derived_td,
        fetched: 0,
        held_before: 0,
        attempted: 0,
        http_exchanges: 0,
        estimated_cu: None,
    })
}

#[cfg(test)]
mod fixture;
#[cfg(test)]
mod tests;
