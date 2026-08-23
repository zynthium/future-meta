pub mod announcement;
pub mod baseline;
pub mod coverage;
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
        #[arg(long)]
        patch: Option<PathBuf>,
    },
    UpdateLatest {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        require_seed: bool,
    },
    MigrateContractSpecs {
        #[arg(long)]
        db: PathBuf,
    },
    DiagnoseLatest {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    ScanAnnouncements {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        reconcile_htfc: bool,
    },
    StageOfficial {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        input: PathBuf,
    },
    RetainOfficialSnapshot {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        canonical_url: String,
        #[arg(long)]
        input: PathBuf,
    },
    ApplyVerifiedOfficial {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        evidence_db: PathBuf,
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
    AuditCoverage {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        from: String,
        #[arg(long)]
        through: String,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        strict: bool,
    },
}

/// Parse CLI arguments and dispatch the selected daemon command.
///
/// # Errors
///
/// Returns an error if the selected command fails.
#[allow(clippy::too_many_lines)]
pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Discover { out } => source::discover_to_file(&out),
        Command::ImportV11Baseline {
            db,
            input,
            metadata_db,
            patch,
        } => {
            let imported = if let Some(patch) = patch {
                baseline::import_v11_baseline_with_patches(&db, &input, &metadata_db, &patch)?
            } else {
                baseline::import_v11_baseline(&db, &input, &metadata_db)?
            };
            eprintln!(
                "V11 baseline imported: rows={} contracts={} sha256={}",
                imported.rows, imported.contracts, imported.source_sha256
            );
            Ok(())
        }
        Command::SeedHistory { db, force_full } => refresh::refresh(&db, force_full),
        Command::UpdateLatest { db, require_seed } => refresh::update_latest(&db, require_seed),
        Command::MigrateContractSpecs { db } => {
            let mut conn = self::db::connect(&db)?;
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let changed = self::db::migrate_known_contract_spec_history(&mut conn, &observed_at)?;
            eprintln!("contract specification history migrated: contracts={changed}");
            Ok(())
        }
        Command::DiagnoseLatest { db, out } => {
            let diagnosis = refresh::diagnose_latest(&db, &out)?;
            eprintln!(
                "latest diagnostic: rows={} rejected={} out={}",
                diagnosis.qihuo_rows,
                diagnosis.diagnostics.len(),
                out.display()
            );
            Ok(())
        }
        Command::ScanAnnouncements { db, reconcile_htfc } => {
            let conn = self::db::connect(&db)?;
            self::db::ensure_schema(&conn)?;
            let transport = announcement::HttpAnnouncementTransport::new()?;
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let summary =
                announcement::scan_announcements(&conn, &transport, &observed_at, reconcile_htfc)?;
            eprintln!(
                "announcement scan: sources={:?} fallback={} documents={} candidates={}",
                summary.sources, summary.used_fallback, summary.documents, summary.candidates
            );
            Ok(())
        }
        Command::StageOfficial { db, input } => {
            let json = std::fs::read_to_string(input)?;
            let staged = official::stage_adjustments_json(&db, &json)?;
            eprintln!(
                "official adjustments staged: adjustments={} verified={}",
                staged.adjustments, staged.verified
            );
            Ok(())
        }
        Command::RetainOfficialSnapshot {
            db,
            canonical_url,
            input,
        } => {
            let body = std::fs::read_to_string(input)?;
            let conn = self::db::connect(&db)?;
            let fetched_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let inserted = self::db::record_official_document_snapshot(
                &conn,
                &canonical_url,
                &body,
                &fetched_at,
            )?;
            eprintln!(
                "official document snapshot retained: inserted={inserted} url={canonical_url}"
            );
            Ok(())
        }
        Command::ApplyVerifiedOfficial { db, evidence_db } => {
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let applied = official::apply_verified_adjustments(&db, &evidence_db, &observed_at)?;
            eprintln!(
                "verified official adjustments applied: adjustments={} resolved_candidates={}",
                applied.adjustments, applied.resolved_candidates
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
        Command::AuditCoverage {
            db,
            from,
            through,
            out,
            strict,
        } => {
            let boundary = coverage::CoverageBoundary::parse(&from, &through)?;
            let report = coverage::audit_history_coverage_to_path(&db, boundary, &out, strict)?;
            eprintln!(
                "coverage audit: contracts={} complete={} findings={} out={}",
                report.contracts,
                report.complete_contracts,
                report.findings.len(),
                out.display()
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn scan_announcements_cli_enables_explicit_htfc_reconciliation() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "scan-announcements",
            "--db",
            "data/future-meta.sqlite",
            "--reconcile-htfc",
        ])
        .unwrap();

        match cli.command {
            Command::ScanAnnouncements { reconcile_htfc, .. } => assert!(reconcile_htfc),
            _ => panic!("expected scan-announcements command"),
        }
    }

    #[test]
    fn diagnose_latest_cli_requires_review_database_and_output_path() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "diagnose-latest",
            "--db",
            "/tmp/review.sqlite",
            "--out",
            "/tmp/latest-diagnostics.json",
        ])
        .unwrap();
        match cli.command {
            Command::DiagnoseLatest { db, out } => {
                assert_eq!(db, PathBuf::from("/tmp/review.sqlite"));
                assert_eq!(out, PathBuf::from("/tmp/latest-diagnostics.json"));
            }
            _ => panic!("expected diagnose-latest command"),
        }
    }

    #[test]
    fn migrate_contract_specs_cli_targets_one_database() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "migrate-contract-specs",
            "--db",
            "data/future-meta.sqlite",
        ])
        .unwrap();

        match cli.command {
            Command::MigrateContractSpecs { db } => {
                assert_eq!(db, PathBuf::from("data/future-meta.sqlite"));
            }
            _ => panic!("expected migrate-contract-specs command"),
        }
    }

    #[test]
    fn apply_verified_official_cli_requires_history_and_evidence_databases() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "apply-verified-official",
            "--db",
            "data/future-meta.sqlite",
            "--evidence-db",
            "data/official-evidence.sqlite",
        ])
        .unwrap();

        match cli.command {
            Command::ApplyVerifiedOfficial { db, evidence_db } => {
                assert_eq!(db, PathBuf::from("data/future-meta.sqlite"));
                assert_eq!(evidence_db, PathBuf::from("data/official-evidence.sqlite"));
            }
            _ => panic!("expected apply-verified-official command"),
        }
    }

    #[test]
    fn retain_official_snapshot_cli_requires_review_database_url_and_input() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "retain-official-snapshot",
            "--db",
            "/tmp/review.sqlite",
            "--canonical-url",
            "https://www.czce.com.cn/cn/DFSStaticFiles/Future/2026/20260608/FutureDataClearParams.htm",
            "--input",
            "/tmp/params.htm",
        ])
        .unwrap();

        match cli.command {
            Command::RetainOfficialSnapshot {
                db,
                canonical_url,
                input,
            } => {
                assert_eq!(db, PathBuf::from("/tmp/review.sqlite"));
                assert_eq!(
                    canonical_url,
                    "https://www.czce.com.cn/cn/DFSStaticFiles/Future/2026/20260608/FutureDataClearParams.htm"
                );
                assert_eq!(input, PathBuf::from("/tmp/params.htm"));
            }
            _ => panic!("expected retain-official-snapshot command"),
        }
    }

    #[test]
    fn coverage_cli_requires_boundary_output_and_strict_flag() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "audit-coverage",
            "--db",
            "data/future-meta.sqlite",
            "--from",
            "2020-01-01",
            "--through",
            "2026-08-24",
            "--out",
            "coverage.json",
            "--strict",
        ])
        .unwrap();

        match cli.command {
            Command::AuditCoverage {
                db,
                from,
                through,
                out,
                strict,
            } => {
                assert_eq!(db, PathBuf::from("data/future-meta.sqlite"));
                assert_eq!(from, "2020-01-01");
                assert_eq!(through, "2026-08-24");
                assert_eq!(out, PathBuf::from("coverage.json"));
                assert!(strict);
            }
            _ => panic!("expected audit-coverage command"),
        }
    }
}
