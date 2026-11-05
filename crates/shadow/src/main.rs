//! Shadow: the ingesters.
//!
//! Six sources (RFC-0001 §11) - archive JSON-RPC, Reth static files, Erigon `.seg` snapshots, era1,
//! Firehose merged blocks, and a Reth ExEx for the steady state - all producing the same relic
//! format.
//!
//! `shadow plan` prints the relics a block range covers. `shadow seal --source era1` reads one
//! era1 file and writes one relic; `--source rpc` journals and seals one relic from an archive
//! JSON-RPC endpoint. The other four sources are still skeleton.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use legacy_format::{relic, SPEC_VERSION};

mod decode;
mod era1;
mod rpc;

#[derive(Parser)]
#[command(
    name = "shadow",
    version,
    about = "The Legacy: transcode chain history into sealed relics"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Source {
    /// Archive JSON-RPC. Bootstrapping only: slow, rate-limited, and holds the raw bytes.
    Rpc,
    /// Reth static files (NippyJar segments).
    Reth,
    /// Erigon 3 block snapshots (`.seg`). State domains are out of scope.
    Erigon,
    /// era1 files. Pre-merge only, and the accumulator root carries into the manifest.
    Era1,
    /// Firehose merged blocks. The only source with call trees, so the only traces source.
    Firehose,
    /// A Reth Execution Extension. The continuous producer, sealing at finality.
    Exex,
}

// Parsed once per process; boxing the larger variant would only complicate the clap derive.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Command {
    /// Print the relics a block range covers, and where they live.
    Plan {
        #[arg(long)]
        from: u64,
        #[arg(long)]
        to: u64,
        #[arg(long, default_value_t = 1)]
        chain_id: u64,
        /// Provider profile; adds an offline request and CU estimate per relic.
        #[arg(long)]
        profile: Option<PathBuf>,
    },

    /// Transcode one source range into a relic.
    Seal {
        #[arg(long, value_enum)]
        source: Source,

        #[arg(long)]
        from: u64,

        #[arg(long)]
        to: u64,

        /// Era1 file. Required for `--source era1`.
        #[arg(long)]
        file: Option<PathBuf>,

        /// Empty directory the relic is written into. Required for `--source era1`.
        #[arg(long)]
        out: Option<PathBuf>,

        /// Predecessor manifest. Required for any relic after genesis, unless `--detached`.
        #[arg(long)]
        after: Option<PathBuf>,

        /// Start a declared detached pact segment here instead of linking to a predecessor
        /// (RFC-0001 §8.4.1). `solo clean` reports the link to genesis as not established.
        #[arg(long, conflicts_with = "after")]
        detached: bool,

        /// Recorded on the manifest. An era1 file does not carry a chain id.
        #[arg(long, default_value_t = 1)]
        chain_id: u64,

        /// Recorded on the manifest. An era1 file does not carry a silo name.
        #[arg(long, default_value = "ethereum")]
        silo: String,

        /// JSON-RPC endpoint for `--source rpc`. Read from `SHADOW_RPC_URL` when omitted, so a
        /// key-bearing URL need not sit in shell history. Never printed.
        #[arg(long, env = "SHADOW_RPC_URL", hide_env_values = true)]
        endpoint: Option<String>,

        /// Bearer token for `--source rpc`, from `SHADOW_RPC_BEARER` only, so it never sits in
        /// argv or shell history.
        #[arg(skip = std::env::var("SHADOW_RPC_BEARER").ok().filter(|t| !t.is_empty()))]
        bearer: Option<String>,

        /// Provider profile (TOML) for `--source rpc`.
        #[arg(long)]
        profile: Option<PathBuf>,

        /// Journal directory for `--source rpc`; a rerun with the same journal resumes.
        #[arg(long)]
        journal: Option<PathBuf>,

        /// Cap on logical method attempts, probes and retries included.
        #[arg(long)]
        budget_requests: Option<u64>,

        /// Cap on estimated billing units; needs `billing.cu_per_method` in the profile.
        #[arg(long)]
        budget_cu: Option<u64>,

        /// Concurrent workers. Defaults to the profile's limit, else 4.
        #[arg(long)]
        concurrency: Option<usize>,

        /// Requests per second. Overrides the profile.
        #[arg(long)]
        rps: Option<f64>,

        /// Further attempts for a block after a transient or invalid response.
        #[arg(long, default_value_t = 3)]
        retries: u32,

        /// `--source rpc` only: fetch and journal the relic, seal nothing. Needs no `--after`
        /// or `--out`; a later run over the same journal seals without the network.
        #[arg(long)]
        journal_only: bool,

        /// `--source rpc` only: total difficulty of the block before the relic, as hex. For a
        /// provider that serves no pre-merge `totalDifficulty`; the relic's own values are then
        /// derived from header difficulties. The base is not header-committed.
        #[arg(long)]
        total_difficulty_base: Option<String>,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("shadow: {e}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Command::Plan {
            from,
            to,
            chain_id,
            profile,
        } => {
            let profile = profile
                .map(|p| rpc::profile::Profile::load(&p))
                .transpose()?;
            if to < from {
                return Err(format!("block range {from}..{to} runs backwards").into());
            }
            let indices = relic::relics_for_range(from, to);
            let (first, last) = (*indices.start(), *indices.end());
            println!(
                "blocks {from}..={to} cover {} relic(s), {first}..={last}",
                last - first + 1
            );
            for index in indices {
                let range = relic::relic_range(index);
                // A relic is sealed whole or not at all, so a range that stops mid-relic is a
                // partial one and saying so now is cheaper than discovering it after the transcode.
                let partial = if range.start < from || range.end > to {
                    "  (partial: the relic is sealed only once its whole range is past finality)"
                } else {
                    ""
                };
                println!(
                    "  {}  blocks {range}  {}{partial}",
                    relic::relic_dir_name(index),
                    relic::relic_prefix(SPEC_VERSION, chain_id, index)
                );
                if let Some(profile) = &profile {
                    let requests = rpc::profile::Profile::requests_for(relic::BLOCKS_PER_RELIC);
                    let cu = profile
                        .billing
                        .cu_per_method
                        .map_or("unpriced".into(), |per| format!("{} CU", requests * per));
                    println!(
                        "             rpc via {}: {requests} method calls before retries, {cu}",
                        profile.provider.alias
                    );
                }
            }
            Ok(())
        }

        Command::Seal {
            source,
            from,
            to,
            file,
            out,
            after,
            detached,
            chain_id,
            silo,
            endpoint,
            bearer,
            profile,
            journal,
            budget_requests,
            budget_cu,
            concurrency,
            rps,
            retries,
            journal_only,
            total_difficulty_base,
        } => {
            if source == Source::Rpc {
                let mut profile =
                    rpc::profile::Profile::load(&profile.ok_or("rpc seal needs --profile")?)?;
                if let Some(rps) = rps {
                    profile.limits.requests_per_second = Some(rps);
                }
                let request = rpc::SealRequest {
                    endpoint: endpoint.ok_or("rpc seal needs --endpoint or SHADOW_RPC_URL")?,
                    bearer,
                    concurrency: concurrency.or(profile.limits.concurrency).unwrap_or(4),
                    profile,
                    journal: journal.ok_or("rpc seal needs --journal")?,
                    out: match out {
                        Some(out) => out,
                        None if journal_only => PathBuf::new(),
                        None => return Err("rpc seal needs --out".into()),
                    },
                    from,
                    to,
                    chain_id,
                    silo,
                    predecessor: after,
                    detached,
                    budget_requests,
                    budget_cu,
                    retries,
                    journal_only,
                    total_difficulty_base: total_difficulty_base
                        .map(|t| {
                            let digits = t.trim_start_matches("0x").trim_start_matches('0');
                            let padded = if digits.len() % 2 == 1 {
                                format!("0{digits}")
                            } else {
                                digits.to_string()
                            };
                            hex::decode(padded)
                        })
                        .transpose()
                        .map_err(|e| format!("--total-difficulty-base: {e}"))?,
                };
                if journal_only {
                    let run = rpc::journal_only(&request)?;
                    println!(
                        "relic       {}  journaled {}/{} blocks ({} fetched now)",
                        run.relic_index,
                        run.held_after,
                        relic::BLOCKS_PER_RELIC,
                        run.fetched
                    );
                    println!(
                        "requests    {} method attempts in {} HTTP exchanges",
                        run.attempted, run.http_exchanges
                    );
                    println!("sealed      nothing; rerun without --journal-only to seal from the journal");
                    return Ok(());
                }
                report_rpc(&rpc::seal(&request)?);
                return Ok(());
            }
            if source != Source::Era1 {
                return Err(format!(
                    "the {source:?} shadow is not implemented yet (RFC-0001 §11); \
                     blocks {from}..={to} stay where they are"
                )
                .into());
            }
            let sealed = era1::seal(&era1::SealRequest {
                file: file.ok_or("era1 seal needs --file")?,
                out: out.ok_or("era1 seal needs --out")?,
                from,
                to,
                chain_id,
                silo,
                predecessor: after,
                detached,
            })?;
            report(&sealed);
            Ok(())
        }
    }
}

fn report(sealed: &era1::Sealed) {
    println!(
        "relic       {}  ({})",
        sealed.relic_index,
        relic::relic_dir_name(sealed.relic_index)
    );
    println!("blocks      {}..={}", sealed.start, sealed.end);
    println!("chain       {}", sealed.chain_id);
    println!("accumulator {}", sealed.accumulator.to_hex());
    println!("pact root   {}", sealed.pact_root.to_hex());
    println!("directory   {}", sealed.directory.display());
    println!("wrote       headers, transactions, receipts, logs");
    println!(
        "checked     era1 framing and block index, accumulator root, header round-trip, \
         ommers hash, transaction and receipt tries, blooms, execution gas"
    );
    println!(
        "NOT checked signature fork rules (sender was recovered into `from`), \
         fee or contract-address agreement, finality, checkpoint anchor"
    );
    if sealed.chain_id != 1 {
        println!(
            "note        solo clean repeats header hashes and trie roots for chain id 1 only; \
             chain {} leaves that profile unchecked",
            sealed.chain_id
        );
    }
}

fn report_rpc(sealed: &rpc::Sealed) {
    println!(
        "relic       {}  ({})",
        sealed.relic_index,
        relic::relic_dir_name(sealed.relic_index)
    );
    println!("blocks      {}..={}", sealed.start, sealed.end);
    println!("chain       {}", sealed.chain_id);
    println!("pact root   {}", sealed.pact_root.to_hex());
    println!("directory   {}", sealed.directory.display());
    println!("wrote       {}", sealed.tables.join(", "));
    println!(
        "rows        {} transactions, {} withdrawals",
        sealed.transactions,
        sealed.withdrawals.map_or("no".into(), |n| n.to_string())
    );
    println!(
        "requests    {} method attempts in {} HTTP exchanges; {} blocks fetched, {} already journaled",
        sealed.attempted, sealed.http_exchanges, sealed.fetched, sealed.held_before
    );
    if sealed.derived_total_difficulty {
        println!(
            "derived     total_difficulty as a running sum of header difficulties from \
             --total-difficulty-base; the base itself is not header-committed"
        );
    }
    if let Some(cu) = sealed.estimated_cu {
        println!("estimate    {cu} CU by the profile's price, not an invoice");
    }
    println!("shadow      {}", rpc::SHADOW_CHECKS);
    println!(
        "NOT checked signature fork rules, fee derivation for blob gas (kept as served), \
         uncle headers beyond an empty-list check, canonical-chain membership, checkpoint anchor"
    );
    println!("note        these are the Shadow's checks on its input; `solo clean --files` is the claim about the files");
}
