//! Solo: the serving binary.
//!
//! The finished article (RFC-0001 §13) is a splitting proxy. What exists is sealed-only JSON-RPC
//! over a local corpus, with no upstream, and `solo clean`.
//!
//! `solo clean` recomputes the pact chain; `--files` adds local byte integrity and implemented
//! table checks. Both say exactly which checks they did and did not perform. A
//! report that implies more than it verified would be worse than no report.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use legacy_format::{manifest::Manifest, pact, relic, SPEC_VERSION};

use solo::clean_files;

#[derive(Parser)]
#[command(
    name = "solo",
    version,
    about = "The Legacy: serve sealed history, verify what you are served"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve a local sealed corpus over JSON-RPC. No upstream.
    Serve {
        #[arg(long, default_value = "solo.toml")]
        config: PathBuf,
    },

    /// Verify a run of relic manifests: structure, linkage, and the pact chain.
    Clean {
        /// Manifest files, in ascending relic order.
        manifests: Vec<PathBuf>,

        /// The already-sealed manifest this run continues from. Omit only when the run starts at
        /// the genesis relic.
        #[arg(long)]
        after: Option<PathBuf>,

        /// Emit the report as JSON rather than prose.
        #[arg(long)]
        json: bool,

        /// Check local table files beside each manifest: bytes, footer counts, and implemented
        /// table codecs. Chain ID 1 also verifies available transaction, receipt and withdrawal
        /// roots. A manifest that carries `era1_accumulator_root` is checked against header
        /// hashes and total difficulties. Does not verify finality, signatures or checkpoint trust.
        #[arg(long)]
        files: bool,
    },

    /// Where a block lives: relic index, block range, object prefix.
    Relic {
        block: u64,

        #[arg(long, default_value_t = 1)]
        chain_id: u64,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("solo: {e}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Command::Serve { config } => solo::serve(&config),

        Command::Relic { block, chain_id } => {
            let index = relic::relic_index(block);
            let range = relic::relic_range(index);
            println!("block       {block}");
            println!("relic       {index}  ({})", relic::relic_dir_name(index));
            println!("blocks      {range}");
            println!(
                "prefix      {}",
                relic::relic_prefix(SPEC_VERSION, chain_id, index)
            );
            Ok(())
        }

        Command::Clean {
            manifests,
            after,
            json,
            files,
        } => clean(&manifests, after.as_deref(), json, files),
    }
}

fn clean(
    paths: &[PathBuf],
    after: Option<&std::path::Path>,
    json: bool,
    files: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if paths.is_empty() {
        return Err("no manifests given".into());
    }

    let previous = after.map(load).transpose()?;
    let manifests: Vec<Manifest> = paths.iter().map(|p| load(p)).collect::<Result<_, _>>()?;

    let head = pact::verify_chain(previous.as_ref(), &manifests)?;
    let anchor = match pact::detached_anchor(previous.as_ref(), &manifests) {
        Some(index) => format!(
            "not checked (relic {index} declares a detached pact start; its link to genesis is not established)"
        ),
        None if previous.is_some() => "predecessor context from --after, not re-verified".into(),
        None => "pass (anchored at the genesis relic)".into(),
    };
    let local_report = if files {
        clean_files::check(paths, &manifests)?
    } else {
        clean_files::LocalReport::default()
    };
    let file_reports = &local_report.files;
    let tx_receipt_links =
        clean_files::summarize(&local_report.relics, |r| r.transaction_receipt_links);
    let log_tx_links = clean_files::summarize(&local_report.relics, |r| r.log_transaction_links);
    let log_receipt_links = clean_files::summarize(&local_report.relics, |r| r.log_receipt_links);
    let receipt_blooms = clean_files::summarize(&local_report.relics, |r| r.receipt_blooms);
    let header_blooms = clean_files::summarize(&local_report.relics, |r| r.header_blooms);
    let receipt_gas = clean_files::summarize(&local_report.relics, |r| r.receipt_gas);
    let transactions_root = clean_files::summarize(&local_report.relics, |r| r.transactions_root);
    let receipts_root = clean_files::summarize(&local_report.relics, |r| r.receipts_root);
    let withdrawals_root = clean_files::summarize(&local_report.relics, |r| r.withdrawals_root);
    let era1_accumulator = if !files {
        "not checked (no relic data read)"
    } else {
        let absent = "not checked (manifest has no era1_accumulator_root)";
        let values: Vec<_> = local_report
            .relics
            .iter()
            .map(|relic| relic.era1_accumulator)
            .collect();
        if values.iter().all(|status| *status == "pass") {
            "pass"
        } else if values.iter().all(|status| *status == absent) {
            absent
        } else {
            clean_files::summarize(&local_report.relics, |relic| relic.era1_accumulator)
        }
    };
    let byte_status = if files {
        "pass"
    } else {
        "not checked (no relic data read)"
    };
    let content_status = if !files {
        "not checked (no relic data read)"
    } else if file_reports.iter().all(|r| r.checks.content_hash == "pass") {
        "pass"
    } else if file_reports.iter().all(|r| r.checks.content_hash != "pass") {
        "not checked (no implemented table codec in these manifests)"
    } else {
        "partial (see per-file checks; some table codecs not implemented)"
    };
    let header_hash_status = if !files {
        "not checked (no relic data read)"
    } else if file_reports
        .iter()
        .filter(|r| r.table == legacy_format::Table::Headers)
        .all(|r| r.checks.header_hashes == "pass")
    {
        "pass"
    } else {
        "not checked (unsupported chain header profile)"
    };
    let traces = manifests
        .iter()
        .filter(|m| m.has_unverifiable_tier())
        .count();

    if json {
        let report = serde_json::json!({
            "chain_id": manifests[0].chain_id,
            "relics": manifests.len(),
            "block_range": {
                "start": manifests[0].block_range.start,
                "end": manifests[manifests.len() - 1].block_range.end,
            },
            "checks": {
                "manifest_structure": "pass",
                "relic_linkage": "pass",
                "pact_chain": "pass",
                "pact_anchor": anchor,
                "file_hashes": byte_status,
                "file_sizes": byte_status,
                "parquet_counts": byte_status,
                "content_hashes": content_status,
                "header_coverage": byte_status,
                "stored_header_linkage": byte_status,
                "manifest_boundary": byte_status,
                "header_hashes": header_hash_status,
                "header_linkage": header_hash_status,
                "consensus_rules": "not checked (including fork activation schedule)",
                "table_completeness": "not checked (not implemented)",
                "producer_signatures": "not checked (not implemented)",
                "era1_accumulator": era1_accumulator,
                "finality": "not checked (not implemented)",
                "index_sidecars": "not checked (not implemented)",
                "transaction_envelopes": "not checked (field RLP semantics and raw/structured agreement not implemented; see transactions_root for raw envelope profile/hash)",
                "transaction_signatures": "not checked (signature validity and sender recovery not implemented)",
                "transactions_root": transactions_root,
                "transaction_receipt_links": tx_receipt_links,
                "log_transaction_links": log_tx_links,
                "log_receipt_links": log_receipt_links,
                "receipt_blooms": receipt_blooms,
                "receipt_consistency": "not checked (fees and other derived fields not implemented)",
                "receipt_gas": receipt_gas,
                "header_blooms": header_blooms,
                "receipts_root": receipts_root,
                "withdrawals_root": withdrawals_root,
                "checkpoint_anchor": "not checked (not implemented)",
                "traces": if traces > 0 {
                    "unverifiable (not header-committed)"
                } else {
                    "n/a (no traces tier)"
                },
            },
            "pact_root": head.to_hex(),
            "files": file_reports,
            "relic_checks": local_report.relics,
            "scope": "requested manifests only; --after supplies predecessor context, not verified file coverage",
            "cleaned_by": concat!("solo/", env!("CARGO_PKG_VERSION")),
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "{} relic(s), blocks {}..={}, chain {}",
            manifests.len(),
            manifests[0].block_range.start,
            manifests[manifests.len() - 1].block_range.end,
            manifests[0].chain_id
        );
        println!("pact root   {head}");
        println!("checked     manifest structure, relic linkage, pact chain");
        if anchor.starts_with("not checked") {
            println!(
                "NOT checked pact anchor: {}",
                anchor
                    .trim_start_matches("not checked (")
                    .trim_end_matches(')')
            );
        } else {
            println!("pact anchor {anchor}");
        }
        if files {
            println!("checked     local file sizes, BLAKE3 hashes, Parquet footer counts");
            println!(
                "checked     complete header coverage, stored parent links, manifest boundaries"
            );
            for file in file_reports {
                println!(
                    "table       relic {} {}: schema/rows/content hash/block bounds: {}",
                    file.relic_index, file.name, file.checks.content_hash
                );
            }
        } else {
            println!("NOT checked file sizes, file hashes, Parquet metadata or table contents");
        }
        println!("header hashes/linkage: {header_hash_status} (Ethereum layout through Prague; requested files only)");
        println!("NOT checked consensus rules/fork schedule, other table completeness, checkpoint anchor, producer signatures");
        println!("transaction trie roots: {transactions_root}");
        for relic in &local_report.relics {
            println!(
                "relic {} transaction trie roots: {}",
                relic.relic_index, relic.transactions_root
            );
        }
        println!("NOT checked transaction field RLP semantics, raw/structured agreement, signatures or sender recovery");
        println!("transaction/receipt links: {tx_receipt_links}");
        println!("log/transaction links: {log_tx_links}");
        println!("log/receipt links: {log_receipt_links}");
        for relic in &local_report.relics {
            println!(
                "relic {} links: transaction/receipt {}; log/transaction {}; log/receipt {}",
                relic.relic_index,
                relic.transaction_receipt_links,
                relic.log_transaction_links,
                relic.log_receipt_links
            );
        }
        println!("receipt blooms: {receipt_blooms}");
        for relic in &local_report.relics {
            println!(
                "relic {} receipt blooms: {}",
                relic.relic_index, relic.receipt_blooms
            );
        }
        println!("header blooms: {header_blooms}");
        for relic in &local_report.relics {
            println!(
                "relic {} header blooms: {}",
                relic.relic_index, relic.header_blooms
            );
        }
        println!("receipt gas accounting: {receipt_gas}");
        for relic in &local_report.relics {
            println!(
                "relic {} receipt gas accounting: {}",
                relic.relic_index, relic.receipt_gas
            );
        }
        println!("receipt trie roots: {receipts_root}");
        for relic in &local_report.relics {
            println!(
                "relic {} receipt trie roots: {}",
                relic.relic_index, relic.receipts_root
            );
        }
        println!("withdrawal trie roots: {withdrawals_root}");
        for relic in &local_report.relics {
            println!(
                "relic {} withdrawal trie roots: {}",
                relic.relic_index, relic.withdrawals_root
            );
        }
        println!("NOT checked receipt fees or other derived fields");
        println!("era1 accumulator: {era1_accumulator}");
        for relic in &local_report.relics {
            println!(
                "relic {} era1 accumulator: {}",
                relic.relic_index, relic.era1_accumulator
            );
        }
        println!("NOT checked finality, index sidecars");
        println!("scope       requested manifests only; --after supplies predecessor context, not verified file coverage");
        if traces > 0 {
            println!(
                "note        {traces} relic(s) carry a traces tier, which no header commits to \
                 and nothing here can verify"
            );
        }
    }
    Ok(())
}

fn load(path: &std::path::Path) -> Result<Manifest, Box<dyn std::error::Error>> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let manifest: Manifest =
        serde_json::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    Ok(manifest)
}
