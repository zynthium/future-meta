pub mod announcement;
pub mod baseline;
pub mod cffex_metadata;
pub mod contract_base_info;
pub mod coverage;
pub mod czce;
pub mod db;
pub mod dce;
pub mod export;
pub mod gfex;
pub mod hash;
pub mod ine;
pub mod jin10;
pub mod latest;
pub mod official;
pub mod official_history;
pub mod official_metadata;
pub mod parse;
pub mod product_spec;
pub mod refresh;
pub mod shfe;
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
    ImportCzceParameters {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        snapshot_dir: PathBuf,
        #[arg(long)]
        from: String,
    },
    ImportDceParameters {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        snapshot_dir: PathBuf,
        #[arg(long)]
        from: String,
    },
    ImportDceCalendar {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        snapshot_dir: PathBuf,
    },
    ImportGfexParameters {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        snapshot_dir: PathBuf,
        #[arg(long)]
        from: String,
    },
    ImportGfexCalendar {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        snapshot_dir: PathBuf,
    },
    ImportIneParameters {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        close_today_rules: PathBuf,
        #[arg(long)]
        snapshot_dir: PathBuf,
        #[arg(long)]
        from: String,
    },
    ImportShfeParameters {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        parameter_manifest: PathBuf,
        #[arg(long)]
        close_today_rules: PathBuf,
        #[arg(long)]
        snapshot_dir: PathBuf,
        #[arg(long)]
        from: String,
        #[arg(long)]
        through: String,
    },
    ImportOfficialHistory {
        #[arg(long)]
        db: PathBuf,
        #[arg(long = "input")]
        inputs: Vec<PathBuf>,
        #[arg(long)]
        evidence_db: Option<PathBuf>,
        #[arg(long)]
        exchange: Option<String>,
        #[arg(long)]
        snapshot_dir: PathBuf,
        #[arg(long)]
        from: String,
        #[arg(long)]
        through: String,
    },
    ImportOfficialMetadata {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        snapshot_dir: PathBuf,
    },
    ImportCffexMetadata {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        product_manifest: PathBuf,
        #[arg(long)]
        calendar_manifest: PathBuf,
        #[arg(long)]
        snapshot_dir: PathBuf,
    },
    ImportContractBaseInfo {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        exchange: String,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        snapshot_dir: PathBuf,
    },
    ImportProductSpecs {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        exchange: String,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        snapshot_dir: PathBuf,
        #[arg(long)]
        from: String,
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
        Command::ImportCzceParameters {
            db,
            manifest,
            snapshot_dir,
            from,
        } => {
            let from = coverage::CoverageBoundary::parse(&from, &from)?.from;
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let result = czce::import_daily_parameters(&czce::CzceParameterImportOptions {
                history_db: db,
                manifest,
                snapshot_dir,
                from,
                observed_at,
            })?;
            eprintln!(
                "CZCE parameters imported: snapshots={} contracts={} versions={}",
                result.snapshots, result.contracts, result.versions
            );
            Ok(())
        }
        Command::ImportDceParameters {
            db,
            manifest,
            snapshot_dir,
            from,
        } => {
            let from = coverage::CoverageBoundary::parse(&from, &from)?.from;
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let result =
                dce::import_daily_settlement_parameters(&dce::DceParameterImportOptions {
                    history_db: db,
                    manifest,
                    snapshot_dir,
                    from,
                    observed_at,
                })?;
            eprintln!(
                "DCE parameters imported: snapshots={} contracts={} versions={}",
                result.snapshots, result.contracts, result.versions
            );
            Ok(())
        }
        Command::ImportDceCalendar {
            db,
            manifest,
            snapshot_dir,
        } => {
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let result = dce::import_trading_calendar_lifecycles(&dce::DceCalendarImportOptions {
                history_db: db,
                manifest,
                snapshot_dir,
                observed_at,
            })?;
            eprintln!(
                "DCE calendar imported: snapshots={} contracts={} evidence_links={}",
                result.snapshots, result.contracts, result.evidence_links
            );
            Ok(())
        }
        Command::ImportGfexParameters {
            db,
            manifest,
            snapshot_dir,
            from,
        } => {
            let from = coverage::CoverageBoundary::parse(&from, &from)?.from;
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let result =
                gfex::import_daily_settlement_parameters(&gfex::GfexParameterImportOptions {
                    history_db: db,
                    manifest,
                    snapshot_dir,
                    from,
                    observed_at,
                })?;
            eprintln!(
                "GFEX parameters imported: snapshots={} contracts={} versions={}",
                result.snapshots, result.contracts, result.versions
            );
            Ok(())
        }
        Command::ImportGfexCalendar {
            db,
            manifest,
            snapshot_dir,
        } => {
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let result =
                gfex::import_trading_calendar_lifecycles(&gfex::GfexCalendarImportOptions {
                    history_db: db,
                    manifest,
                    snapshot_dir,
                    observed_at,
                })?;
            eprintln!(
                "GFEX calendar imported: snapshots={} contracts={} evidence_links={}",
                result.snapshots, result.contracts, result.evidence_links
            );
            Ok(())
        }
        Command::ImportIneParameters {
            db,
            manifest,
            close_today_rules,
            snapshot_dir,
            from,
        } => {
            let from = coverage::CoverageBoundary::parse(&from, &from)?.from;
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let result = ine::import_daily_parameters(&ine::IneParameterImportOptions {
                history_db: db,
                manifest,
                close_today_rules,
                snapshot_dir,
                from,
                observed_at,
            })?;
            eprintln!(
                "INE parameters imported: snapshots={} contracts={} versions={}",
                result.snapshots, result.contracts, result.versions
            );
            Ok(())
        }
        Command::ImportShfeParameters {
            db,
            parameter_manifest,
            close_today_rules,
            snapshot_dir,
            from,
            through,
        } => {
            let boundary = coverage::CoverageBoundary::parse(&from, &through)?;
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let result = shfe::import_monthly_parameters(&shfe::ShfeParameterImportOptions {
                history_db: db,
                parameter_manifest,
                close_today_rules,
                snapshot_dir,
                from: boundary.from,
                through: boundary.through,
                observed_at,
            })?;
            eprintln!(
                "SHFE parameters imported: snapshots={} contracts={} versions={}",
                result.snapshots, result.contracts, result.versions
            );
            Ok(())
        }
        Command::ImportOfficialHistory {
            db,
            inputs,
            evidence_db,
            exchange,
            snapshot_dir,
            from,
            through,
        } => {
            let boundary = coverage::CoverageBoundary::parse(&from, &through)?;
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let result = official_history::import_adjustments(
                &official_history::OfficialHistoryImportOptions {
                    history_db: db,
                    inputs,
                    evidence_db,
                    exchange,
                    snapshot_dir,
                    from: boundary.from,
                    through: boundary.through,
                    observed_at,
                },
            )?;
            eprintln!(
                "official history imported: adjustments={} contracts={} versions={}",
                result.adjustments, result.contracts, result.versions
            );
            Ok(())
        }
        Command::ImportOfficialMetadata {
            db,
            manifest,
            snapshot_dir,
        } => {
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let result = official_metadata::import_contract_metadata(
                &official_metadata::OfficialMetadataImportOptions {
                    history_db: db,
                    manifest,
                    snapshot_dir,
                    observed_at,
                },
            )?;
            eprintln!(
                "official metadata imported: contracts={} specification_versions={}",
                result.contracts, result.specification_versions
            );
            Ok(())
        }
        Command::ImportCffexMetadata {
            db,
            product_manifest,
            calendar_manifest,
            snapshot_dir,
        } => {
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let result = cffex_metadata::import_contract_metadata(
                &cffex_metadata::CffexMetadataImportOptions {
                    history_db: db,
                    product_manifest,
                    calendar_manifest,
                    snapshot_dir,
                    observed_at,
                },
            )?;
            eprintln!(
                "CFFEX metadata imported: contracts={} specification_versions={}",
                result.contracts, result.specification_versions
            );
            Ok(())
        }
        Command::ImportContractBaseInfo {
            db,
            exchange,
            manifest,
            snapshot_dir,
        } => {
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let result = contract_base_info::import_contract_base_info(
                &contract_base_info::ContractBaseInfoImportOptions {
                    history_db: db,
                    exchange: exchange.clone(),
                    manifest,
                    snapshot_dir,
                    observed_at,
                },
            )?;
            eprintln!(
                "{exchange} contract base info imported: snapshots={} contracts={} evidence_links={}",
                result.snapshots, result.contracts, result.evidence_links
            );
            Ok(())
        }
        Command::ImportProductSpecs {
            db,
            exchange,
            manifest,
            snapshot_dir,
            from,
        } => {
            let from = coverage::CoverageBoundary::parse(&from, &from)?.from;
            let observed_at = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)?;
            let result =
                product_spec::import_product_specs(&product_spec::ProductSpecImportOptions {
                    history_db: db,
                    exchange: exchange.clone(),
                    manifest,
                    snapshot_dir,
                    from,
                    observed_at,
                })?;
            eprintln!(
                "{exchange} product specifications imported: products={} contracts={} versions={}",
                result.products, result.contracts, result.versions
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
    fn import_official_history_cli_accepts_multiple_reviewed_inputs() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "import-official-history",
            "--db",
            "/tmp/review.sqlite",
            "--input",
            "/tmp/listings.json",
            "--input",
            "/tmp/changes.json",
            "--snapshot-dir",
            "/tmp/evidence",
            "--exchange",
            "CFFEX",
            "--from",
            "2020-01-01",
            "--through",
            "2026-08-24",
        ])
        .unwrap();
        match cli.command {
            Command::ImportOfficialHistory {
                inputs, exchange, ..
            } => {
                assert_eq!(inputs.len(), 2);
                assert_eq!(exchange.as_deref(), Some("CFFEX"));
            }
            _ => panic!("expected import-official-history command"),
        }
    }

    #[test]
    fn import_official_metadata_cli_requires_evidence_inputs() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "import-official-metadata",
            "--db",
            "/tmp/review.sqlite",
            "--manifest",
            "/tmp/metadata.tsv",
            "--snapshot-dir",
            "/tmp/evidence",
        ])
        .unwrap();
        match cli.command {
            Command::ImportOfficialMetadata {
                db,
                manifest,
                snapshot_dir,
            } => {
                assert_eq!(db, PathBuf::from("/tmp/review.sqlite"));
                assert_eq!(manifest, PathBuf::from("/tmp/metadata.tsv"));
                assert_eq!(snapshot_dir, PathBuf::from("/tmp/evidence"));
            }
            _ => panic!("expected import-official-metadata command"),
        }
    }

    #[test]
    fn import_cffex_metadata_cli_requires_product_and_calendar_manifests() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "import-cffex-metadata",
            "--db",
            "/tmp/review.sqlite",
            "--product-manifest",
            "/tmp/cffex-products.tsv",
            "--calendar-manifest",
            "/tmp/cffex-calendars.tsv",
            "--snapshot-dir",
            "/tmp/evidence",
        ])
        .unwrap();
        match cli.command {
            Command::ImportCffexMetadata {
                db,
                product_manifest,
                calendar_manifest,
                snapshot_dir,
            } => {
                assert_eq!(db, PathBuf::from("/tmp/review.sqlite"));
                assert_eq!(product_manifest, PathBuf::from("/tmp/cffex-products.tsv"));
                assert_eq!(calendar_manifest, PathBuf::from("/tmp/cffex-calendars.tsv"));
                assert_eq!(snapshot_dir, PathBuf::from("/tmp/evidence"));
            }
            _ => panic!("expected import-cffex-metadata command"),
        }
    }

    #[test]
    fn import_contract_base_info_cli_requires_exchange_and_evidence_inputs() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "import-contract-base-info",
            "--db",
            "/tmp/review.sqlite",
            "--exchange",
            "SHFE",
            "--manifest",
            "/tmp/contract-base.tsv",
            "--snapshot-dir",
            "/tmp/evidence",
        ])
        .unwrap();
        match cli.command {
            Command::ImportContractBaseInfo {
                db,
                exchange,
                manifest,
                snapshot_dir,
            } => {
                assert_eq!(db, PathBuf::from("/tmp/review.sqlite"));
                assert_eq!(exchange, "SHFE");
                assert_eq!(manifest, PathBuf::from("/tmp/contract-base.tsv"));
                assert_eq!(snapshot_dir, PathBuf::from("/tmp/evidence"));
            }
            _ => panic!("expected import-contract-base-info command"),
        }
    }

    #[test]
    fn import_product_specs_cli_requires_boundary_and_evidence_inputs() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "import-product-specs",
            "--db",
            "/tmp/review.sqlite",
            "--exchange",
            "INE",
            "--manifest",
            "/tmp/product-spec.tsv",
            "--snapshot-dir",
            "/tmp/evidence",
            "--from",
            "2020-01-01",
        ])
        .unwrap();
        match cli.command {
            Command::ImportProductSpecs { exchange, from, .. } => {
                assert_eq!(exchange, "INE");
                assert_eq!(from, "2020-01-01");
            }
            _ => panic!("expected import-product-specs command"),
        }
    }

    #[test]
    fn import_gfex_parameters_cli_requires_retained_evidence_inputs() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "import-gfex-parameters",
            "--db",
            "/tmp/review.sqlite",
            "--manifest",
            "/tmp/gfex.tsv",
            "--snapshot-dir",
            "/tmp/evidence",
            "--from",
            "2022-12-22",
        ])
        .unwrap();
        match cli.command {
            Command::ImportGfexParameters { from, .. } => {
                assert_eq!(from, "2022-12-22");
            }
            _ => panic!("expected import-gfex-parameters command"),
        }
    }

    #[test]
    fn import_dce_parameters_cli_requires_retained_evidence_inputs() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "import-dce-parameters",
            "--db",
            "/tmp/review.sqlite",
            "--manifest",
            "/tmp/dce.tsv",
            "--snapshot-dir",
            "/tmp/evidence",
            "--from",
            "2020-01-01",
        ])
        .unwrap();
        match cli.command {
            Command::ImportDceParameters {
                db,
                manifest,
                snapshot_dir,
                from,
            } => {
                assert_eq!(db, PathBuf::from("/tmp/review.sqlite"));
                assert_eq!(manifest, PathBuf::from("/tmp/dce.tsv"));
                assert_eq!(snapshot_dir, PathBuf::from("/tmp/evidence"));
                assert_eq!(from, "2020-01-01");
            }
            _ => panic!("expected import-dce-parameters command"),
        }
    }

    #[test]
    fn import_dce_calendar_cli_requires_retained_evidence_inputs() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "import-dce-calendar",
            "--db",
            "/tmp/review.sqlite",
            "--manifest",
            "/tmp/dce-calendar.tsv",
            "--snapshot-dir",
            "/tmp/evidence",
        ])
        .unwrap();
        match cli.command {
            Command::ImportDceCalendar {
                db,
                manifest,
                snapshot_dir,
            } => {
                assert_eq!(db, PathBuf::from("/tmp/review.sqlite"));
                assert_eq!(manifest, PathBuf::from("/tmp/dce-calendar.tsv"));
                assert_eq!(snapshot_dir, PathBuf::from("/tmp/evidence"));
            }
            _ => panic!("expected import-dce-calendar command"),
        }
    }

    #[test]
    fn import_ine_parameters_cli_requires_close_today_rules() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "import-ine-parameters",
            "--db",
            "/tmp/review.sqlite",
            "--manifest",
            "/tmp/ine.tsv",
            "--close-today-rules",
            "/tmp/ine-close-today.tsv",
            "--snapshot-dir",
            "/tmp/evidence",
            "--from",
            "2020-01-01",
        ])
        .unwrap();
        match cli.command {
            Command::ImportIneParameters {
                db,
                manifest,
                close_today_rules,
                snapshot_dir,
                from,
            } => {
                assert_eq!(db, PathBuf::from("/tmp/review.sqlite"));
                assert_eq!(manifest, PathBuf::from("/tmp/ine.tsv"));
                assert_eq!(close_today_rules, PathBuf::from("/tmp/ine-close-today.tsv"));
                assert_eq!(snapshot_dir, PathBuf::from("/tmp/evidence"));
                assert_eq!(from, "2020-01-01");
            }
            _ => panic!("expected import-ine-parameters command"),
        }
    }

    #[test]
    fn import_czce_parameters_cli_requires_retained_evidence_inputs() {
        let cli = Cli::try_parse_from([
            "future-meta-daemon",
            "import-czce-parameters",
            "--db",
            "/tmp/review.sqlite",
            "--manifest",
            "/tmp/czce.tsv",
            "--snapshot-dir",
            "/tmp/evidence",
            "--from",
            "2020-01-01",
        ])
        .unwrap();
        match cli.command {
            Command::ImportCzceParameters {
                db,
                manifest,
                snapshot_dir,
                from,
            } => {
                assert_eq!(db, PathBuf::from("/tmp/review.sqlite"));
                assert_eq!(manifest, PathBuf::from("/tmp/czce.tsv"));
                assert_eq!(snapshot_dir, PathBuf::from("/tmp/evidence"));
                assert_eq!(from, "2020-01-01");
            }
            _ => panic!("expected import-czce-parameters command"),
        }
    }

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
