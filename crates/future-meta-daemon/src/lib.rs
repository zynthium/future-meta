pub mod db;
pub mod export;
pub mod hash;
pub mod latest;
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
    UpdateLatest {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        require_seed: bool,
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
        Command::SeedHistory { db, force_full } => refresh::refresh(&db, force_full),
        Command::UpdateLatest { db, require_seed } => refresh::update_latest(&db, require_seed),
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
