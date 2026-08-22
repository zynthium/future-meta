pub mod baseline;
pub mod db;
pub mod export;
pub mod hash;
pub mod jin10;
pub mod latest;
pub mod official;
pub mod parse;
pub mod refresh;
pub mod source;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "future-meta-daemon")]
#[command(about = "Maintain and export future-meta fee history")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Discover {
        #[arg(long)]
        out: PathBuf,
    },
    SeedHistory {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        force_full: bool,
    },
    ImportV11Baseline {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        metadata_db: PathBuf,
    },
    UpdateLatest {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        require_seed: bool,
    },
    StageOfficial {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        input: PathBuf,
    },
    BackfillJin10 {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
    ValidateJin10 {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    Refresh {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        force_full: bool,
        #[arg(long)]
        require_seed: bool,
    },
    Export {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    Inspect {
        #[arg(long)]
        db: PathBuf,
    },
}

/// Parse CLI arguments and dispatch the selected daemon command.
///
/// # Errors
///
/// Returns an error if the selected command fails.
pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Discover { out } => source::discover_to_file(&out),
        Command::ImportV11Baseline {
            db,
            input,
            metadata_db,
        } => {
            let imported = baseline::import_v11_baseline(&db, &input, &metadata_db)?;
            eprintln!(
                "V11 baseline imported: rows={} contracts={} sha256={}",
                imported.rows, imported.contracts, imported.source_sha256
            );
            Ok(())
        }
        Command::SeedHistory { db, force_full } => refresh::refresh(&db, force_full),
        Command::UpdateLatest { db, require_seed } => refresh::update_latest(&db, require_seed),
        Command::StageOfficial { db, input } => {
            let json = std::fs::read_to_string(input)?;
            let staged = official::stage_adjustments_json(&db, &json)?;
            eprintln!(
                "official adjustments staged: adjustments={} verified={}",
                staged.adjustments, staged.verified
            );
            Ok(())
        }
        Command::BackfillJin10 { db, from, to } => {
            let result = refresh::backfill_jin10(&db, &from, &to)?;
            eprintln!(
                "Jin10 backfill complete: snapshots={} rows={} skipped_invalid_symbols={}",
                result.snapshots, result.rows, result.skipped_invalid_symbols
            );
            Ok(())
        }
        Command::ValidateJin10 { db, from, to, out } => {
            let result = refresh::validate_jin10(&db, &from, &to, out.as_deref())?;
            eprintln!(
                "Jin10 validation: snapshots={} rows={} compared={} mismatches={} skipped_invalid_symbols={} skipped_missing_metadata={}",
                result.snapshots,
                result.jin10_rows,
                result.compared_rows,
                result.mismatch_count,
                result.skipped_invalid_symbols,
                result.skipped_missing_metadata,
            );
            Ok(())
        }
        Command::Refresh {
            db,
            force_full,
            require_seed,
        } => refresh::refresh_with_options(
            &db,
            refresh::RefreshOptions {
                force_full,
                require_seed,
            },
        ),
        Command::Export { db, out } => export::export_archive(&db, &out),
        Command::Inspect { db } => db::inspect(&db),
    }
}
