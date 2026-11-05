//! RFC-0004 M1 gate: budgets, unavailable methods, batch reassembly, corrupt responses, finality
//! and crash/resume, all against the local fixture server.

use std::path::{Path, PathBuf};

use legacy_format::manifest::Manifest;

use super::fixture::{self, Knobs, Server, BUSY};
use super::profile::{Limits, Methods, Profile, Provider};
use super::*;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shadow-rpc-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn profile(batch_max: usize) -> Profile {
    Profile {
        provider: Provider {
            alias: "fixture".into(),
            chain_id: 1,
        },
        methods: Methods {
            batch_max,
            ..Methods::default()
        },
        limits: Limits::default(),
        billing: Default::default(),
    }
}

fn predecessor(dir: &Path) -> PathBuf {
    // A stand-in relic 2078 whose end hash is the fixture's parent_hash_of_start, so the pact link
    // check runs for real. It is not itself chained to genesis; only the link is under test.
    let range = relic::relic_range(fixture::RELIC - 1);
    let mut manifest = Manifest::new(
        1,
        "ethereum",
        fixture::RELIC - 1,
        legacy_format::manifest::Boundary {
            start_block_hash: Hash32::new([0x98; 32]),
            end_block_hash: Hash32::new([0x99; 32]),
            parent_hash_of_start: Hash32::new([0x97; 32]),
        },
    );
    assert_eq!(manifest.block_range.end, range.end);
    manifest.pact_root = Hash32::new([0x42; 32]);
    let path = dir.join("002078.json");
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    path
}

fn request(server: &Server, dir: &Path, profile: Profile) -> SealRequest {
    let range = fixture::range();
    SealRequest {
        endpoint: server.url.clone(),
        bearer: None,
        profile,
        journal: dir.join("journal"),
        out: dir.join("relic"),
        from: range.start,
        to: range.end,
        chain_id: 1,
        silo: "ethereum".into(),
        predecessor: Some(predecessor(dir)),
        detached: false,
        budget_requests: None,
        budget_cu: None,
        concurrency: 4,
        retries: 3,
        journal_only: false,
    }
}

fn held(dir: &Path) -> std::collections::BTreeSet<u64> {
    Journal::open(&dir.join("journal"))
        .unwrap()
        .held(fixture::range())
        .unwrap()
}

#[test]
fn a_full_relic_seals_and_its_tables_verify() {
    let dir = scratch("full");
    let server = fixture::serve(Knobs::default());
    let sealed = seal(&request(&server, &dir, profile(0))).unwrap();
    assert_eq!(sealed.relic_index, fixture::RELIC);
    assert_eq!(sealed.fetched, relic::BLOCKS_PER_RELIC);
    assert_eq!(
        sealed.tables,
        ["headers", "transactions", "receipts", "logs", "withdrawals"]
    );
    // probes + one block and one receipts call per block, nothing else
    assert_eq!(
        sealed.attempted,
        Profile::requests_for(relic::BLOCKS_PER_RELIC)
    );

    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(dir.join("relic/manifest.json")).unwrap()).unwrap();
    manifest.validate().unwrap();
    for file in &manifest.files {
        let bytes = std::fs::read(dir.join("relic").join(&file.name)).unwrap();
        legacy_parquet::verify::verify_file(bytes.into(), file, &manifest).unwrap();
    }
    let txs = legacy_parquet::transactions::read_transactions(
        std::fs::read(dir.join("relic/transactions.parquet"))
            .unwrap()
            .into(),
    )
    .unwrap();
    let types: std::collections::BTreeSet<u8> = txs.iter().map(|t| t.tx_type).collect();
    assert_eq!(types, [0, 1, 2, 3, 4].into());
    // Withdrawals begin partway through, exactly as relic 2079's do on mainnet.
    let withdrawals = legacy_parquet::withdrawals::read_withdrawals(
        std::fs::read(dir.join("relic/withdrawals.parquet"))
            .unwrap()
            .into(),
    )
    .unwrap();
    assert_eq!(withdrawals[0].block_number, fixture::SHAPELLA);
    assert_eq!(withdrawals[0].index, 0);
}

#[test]
fn a_journal_reseals_without_the_network() {
    let dir = scratch("reseal");
    let server = fixture::serve(Knobs::default());
    seal(&request(&server, &dir, profile(16))).unwrap();
    let first = std::fs::read(dir.join("relic/manifest.json")).unwrap();

    let mut again = request(&server, &dir, profile(16));
    again.out = dir.join("relic-again");
    server.knobs.lock().unwrap().served.clear();
    let sealed = seal(&again).unwrap();
    assert_eq!(sealed.fetched, 0);
    assert!(server.knobs.lock().unwrap().served.is_empty());
    // Only the chain id and finality probes, which resumption repeats once.
    assert_eq!(sealed.attempted, super::profile::PROBES);
    assert_eq!(
        first,
        std::fs::read(dir.join("relic-again/manifest.json")).unwrap()
    );
}

#[test]
fn shuffled_batch_replies_are_reassembled_by_id() {
    let dir = scratch("batch");
    let server = fixture::serve(Knobs {
        shuffle_batches: true,
        ..Knobs::default()
    });
    let sealed = seal(&request(&server, &dir, profile(8))).unwrap();
    assert_eq!(sealed.fetched, relic::BLOCKS_PER_RELIC);
    assert!(sealed.http_exchanges < sealed.attempted);
}

#[test]
fn a_missing_receipts_method_fails_the_probe_after_one_block() {
    let dir = scratch("noreceipts");
    let server = fixture::serve(Knobs {
        no_block_receipts: true,
        ..Knobs::default()
    });
    let error = seal(&request(&server, &dir, profile(0))).unwrap_err();
    assert!(matches!(error, Error::Probe(_)), "{error}");
    assert!(held(&dir).is_empty());
    // chain id, finality, one block, one receipts call
    assert_eq!(server.knobs.lock().unwrap().calls, 4);
}

#[test]
fn an_unfittable_relic_is_refused_before_any_request() {
    let dir = scratch("unfit");
    let server = fixture::serve(Knobs::default());
    let mut req = request(&server, &dir, profile(0));
    req.budget_requests = Some(1_000);
    let error = seal(&req).unwrap_err();
    assert!(error.to_string().contains("nothing was fetched"), "{error}");
    assert_eq!(server.knobs.lock().unwrap().calls, 0);
}

#[test]
fn retries_exhaust_the_budget_and_a_rerun_resumes_without_refetching() {
    let dir = scratch("resume");
    let server = fixture::serve(Knobs {
        transient_every: Some(7),
        ..Knobs::default()
    });
    let mut req = request(&server, &dir, profile(0));
    let budget = Profile::requests_for(relic::BLOCKS_PER_RELIC) + 10;
    req.budget_requests = Some(budget);
    let error = seal(&req).unwrap_err();
    assert!(matches!(error, Error::Budget { .. }), "{error}");
    // Reservation happens before dispatch, so the cap is never crossed.
    assert!(server.knobs.lock().unwrap().calls <= budget);
    let after_first = held(&dir);
    assert!(!after_first.is_empty() && (after_first.len() as u64) < relic::BLOCKS_PER_RELIC);
    assert!(!dir.join("relic/manifest.json").exists());

    {
        let mut knobs = server.knobs.lock().unwrap();
        knobs.transient_every = None;
        knobs.served.clear();
    }
    let sealed = seal(&request(&server, &dir, profile(0))).unwrap();
    assert_eq!(
        sealed.fetched + after_first.len() as u64,
        relic::BLOCKS_PER_RELIC
    );
    let knobs = server.knobs.lock().unwrap();
    for n in knobs.served.values().flatten() {
        assert!(!after_first.contains(n), "block {n} was fetched twice");
    }
}

#[test]
fn a_corrupt_response_is_never_journaled() {
    let dir = scratch("corrupt");
    let server = fixture::serve(Knobs {
        corrupt_always: [BUSY].into(),
        ..Knobs::default()
    });
    let error = seal(&request(&server, &dir, profile(0))).unwrap_err();
    assert!(
        matches!(error, Error::Block { number, .. } if number == BUSY),
        "{error}"
    );
    assert!(!held(&dir).contains(&BUSY));
}

#[test]
fn a_one_off_corruption_is_retried_and_the_relic_seals() {
    let dir = scratch("corrupt-once");
    let server = fixture::serve(Knobs {
        corrupt_once: [BUSY].into(),
        ..Knobs::default()
    });
    let sealed = seal(&request(&server, &dir, profile(0))).unwrap();
    assert_eq!(sealed.fetched, relic::BLOCKS_PER_RELIC);
    let block_fetches = server.knobs.lock().unwrap().served["block"]
        .iter()
        .filter(|n| **n == BUSY)
        .count();
    assert_eq!(block_fetches, 2);
}

#[test]
fn a_relic_past_the_finalized_head_is_refused() {
    let dir = scratch("finality");
    let server = fixture::serve(Knobs {
        finalized: Some(fixture::range().end - 1),
        ..Knobs::default()
    });
    let error = seal(&request(&server, &dir, profile(0))).unwrap_err();
    assert!(error.to_string().contains("finality"), "{error}");
    assert!(held(&dir).is_empty());
}

#[test]
fn a_wrong_chain_is_refused_by_the_profile_before_the_network() {
    let dir = scratch("chain");
    let server = fixture::serve(Knobs::default());
    let mut req = request(&server, &dir, profile(0));
    req.chain_id = 10;
    assert!(seal(&req).is_err());
    assert_eq!(server.knobs.lock().unwrap().calls, 0);
}

#[test]
fn quantities_are_strict_and_scalars_tolerate_padding() {
    assert_eq!(wire::quantity("x", "0x0").unwrap(), Vec::<u8>::new());
    assert_eq!(wire::quantity("x", "0x1ff").unwrap(), vec![1, 0xff]);
    assert!(wire::quantity("x", "0x01").is_err());
    assert!(wire::quantity("x", "0x").is_err());
    assert!(wire::quantity("x", "12").is_err());
    assert_eq!(
        wire::scalar(
            "r",
            "0x00000000000000000000000000000000000000000000000000000000000000ab"
        )
        .unwrap(),
        vec![0xab]
    );
}

#[test]
fn journal_only_needs_no_predecessor_and_leaves_progress_behind() {
    let dir = scratch("journal-only");
    let server = fixture::serve(Knobs::default());
    let mut req = request(&server, &dir, profile(0));
    req.predecessor = None;
    req.journal_only = true;
    let run = journal_only(&req).unwrap();
    assert_eq!(run.held_after, relic::BLOCKS_PER_RELIC);
    assert!(!dir.join("relic").exists());
    let progress: Value =
        serde_json::from_slice(&std::fs::read(dir.join("journal/progress.json")).unwrap()).unwrap();
    assert_eq!(progress["state"], "journaled");
    assert_eq!(progress["held"], relic::BLOCKS_PER_RELIC);
    assert_eq!(progress["rejected_responses"], 0);
}

#[test]
fn a_rejected_response_is_counted_in_progress() {
    let dir = scratch("rejected");
    let server = fixture::serve(Knobs {
        corrupt_once: [BUSY].into(),
        ..Knobs::default()
    });
    let mut req = request(&server, &dir, profile(0));
    req.journal_only = true;
    journal_only(&req).unwrap();
    let progress: Value =
        serde_json::from_slice(&std::fs::read(dir.join("journal/progress.json")).unwrap()).unwrap();
    assert_eq!(progress["rejected_responses"], 1);
    assert!(progress["last_rejection"]
        .as_str()
        .unwrap()
        .contains(&BUSY.to_string()));
}

#[test]
fn a_budget_stop_still_writes_provenance() {
    let dir = scratch("provenance");
    let server = fixture::serve(Knobs {
        transient_every: Some(5),
        ..Knobs::default()
    });
    let mut req = request(&server, &dir, profile(0));
    req.budget_requests = Some(Profile::requests_for(relic::BLOCKS_PER_RELIC));
    assert!(matches!(seal(&req), Err(Error::Budget { .. })));
    let provenance: Value =
        serde_json::from_slice(&std::fs::read(dir.join("journal/provenance.json")).unwrap())
            .unwrap();
    assert_eq!(provenance["runs"][0]["state"], "budget exhausted");
}

#[test]
fn a_detached_seal_needs_no_predecessor_and_says_so_in_the_manifest() {
    let dir = scratch("detached");
    let server = fixture::serve(Knobs::default());
    let mut req = request(&server, &dir, profile(0));
    req.predecessor = None;
    req.detached = true;
    seal(&req).unwrap();
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(dir.join("relic/manifest.json")).unwrap()).unwrap();
    assert!(manifest.detached_pact_start);
    assert_eq!(manifest.prev_relic_manifest_hash, Hash32::ZERO);
    legacy_format::pact::verify_chain(None, std::slice::from_ref(&manifest)).unwrap();
}
