#!/usr/bin/env python3
"""Collect only the missing official trading interval and prepare Pages output."""
import argparse
import csv
import json
import shutil
import sqlite3
import subprocess
import sys
from datetime import date, datetime, time, timedelta
from pathlib import Path
from zoneinfo import ZoneInfo

from package_official_inputs import package, restore

ROOT = Path(__file__).resolve().parents[1]


def completed_china_date(now=None):
    now = now or datetime.now(ZoneInfo("Asia/Shanghai"))
    # Daily production runs at 18:45, after exchange settlement publication.
    return now.date() if now.time() >= time(18, 30) else now.date() - timedelta(days=1)


def seed_through(db):
    with sqlite3.connect(f"file:{Path(db).resolve()}?mode=ro", uri=True) as connection:
        reports = connection.execute("select report_json from rebuild_summary").fetchall()
    if len(reports) != 1:
        raise ValueError("Expected exactly one collected rebuild summary")
    report = json.loads(reports[0][0])
    if report.get("source") != "fresh_collector_outputs_no_seed":
        raise ValueError("Seed is not the official collected database")
    return date.fromisoformat(report["through"])


def run(*args):
    command = [str(arg) for arg in args]
    print("+ " + " ".join(command), flush=True)
    subprocess.run(command, cwd=ROOT, check=True)


def daemon(*args):
    run("cargo", "run", "-q", "-p", "future-meta-daemon", "--", *args)


def pending_announcements(db, out):
    with sqlite3.connect(f"file:{Path(db).resolve()}?mode=ro", uri=True) as connection:
        connection.row_factory = sqlite3.Row
        exists = connection.execute("select 1 from sqlite_master where name='announcement_candidates'").fetchone()
        rows = [] if not exists else [dict(row) for row in connection.execute(
            "select c.*, d.title, d.broker_url from announcement_candidates c "
            "join announcement_documents d using(source,article_id) "
            "where c.resolved_at is null order by c.detected_at,c.source,c.article_id")]
    Path(out).write_text(json.dumps({"pending_count": len(rows), "pending": rows}, ensure_ascii=False, indent=2))
    print(f"Supplementary announcement audit: {len(rows)} pending; official updates do not resolve these candidates", flush=True)


def refresh_calendar(evidence, raw, start, through):
    # Calendar is independently fetched and validated before interpreting empty
    # intervals. HTTP errors are never evidence of a holiday.
    from collect_settle_params import RawStore, trading_days
    store = RawStore(raw, None, False)
    days = trading_days(start.isoformat(), through.isoformat(), store)
    rows = {}
    manifests = sorted(evidence.glob("cffex-calendar-*.tsv"))
    if not manifests:
        raise ValueError("Bootstrap lacks official CFFEX calendar manifest")
    # build_lifecycles selects the 2017 manifest first; update that same file.
    manifest = next((p for p in manifests if p.name == "cffex-calendar-2017-2026.tsv"), manifests[0])
    with manifest.open(encoding="utf-8", newline="") as handle:
        for row in csv.DictReader(handle, delimiter="\t"):
            rows[row["month"]] = row
    from package_official_inputs import digest
    for path in (raw / "calendar").glob("*.json"):
        wrapper = json.loads(path.read_text())
        body = store.unpack(wrapper)
        sha = digest(body)
        (evidence / (sha + ".xml")).write_bytes(body)
        month = path.stem
        rows[month] = {"month": month, "canonical_url": wrapper["canonical_url"], "sha256": sha, "raw_path": sha + ".xml"}
    columns = ["month", "canonical_url", "sha256", "raw_path"]
    with manifest.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, columns, delimiter="\t", extrasaction="ignore")
        writer.writeheader()
        writer.writerows(rows[key] for key in sorted(rows))
    return days


def lifecycle_snapshot(evidence):
    snapshots = {}
    for path in evidence.glob("*-lifecycle-*.tsv"):
        with path.open(encoding="utf-8", newline="") as handle:
            reader = csv.DictReader(handle, delimiter="\t")
            snapshots[path.name] = (reader.fieldnames, list(reader))
    return snapshots


def preserve_known_lifecycles(evidence, snapshots):
    # Existing symbols keep their reviewed dates/provenance. Rules may create
    # rows only for newly observed contracts; they cannot relabel confirmed
    # historical expiry dates with a new inference.
    known = {(row["exchange"], row["symbol"]) for _, rows in snapshots.values() for row in rows}
    for path in evidence.glob("*-lifecycle-*.tsv"):
        with path.open(encoding="utf-8", newline="") as handle:
            reader = csv.DictReader(handle, delimiter="\t")
            columns, generated = reader.fieldnames, list(reader)
        previous_columns, previous = snapshots.get(path.name, (columns, []))
        columns = list(dict.fromkeys(previous_columns + columns))
        rows = previous + [row for row in generated if (row["exchange"], row["symbol"]) not in known]
        with path.open("w", encoding="utf-8", newline="") as handle:
            writer = csv.DictWriter(handle, columns, delimiter="\t")
            writer.writeheader()
            writer.writerows(rows)
    for name, (columns, rows) in snapshots.items():
        path = evidence / name
        if not path.exists():
            with path.open("w", encoding="utf-8", newline="") as handle:
                writer = csv.DictWriter(handle, columns, delimiter="\t")
                writer.writeheader()
                writer.writerows(rows)


def update(args):
    work, public = args.work_dir.resolve(), args.public_dir.resolve()
    work.mkdir(parents=True, exist_ok=True)
    diagnostics = work / "diagnostics"
    diagnostics.mkdir(exist_ok=True)
    prior = seed_through(args.db)
    pending_announcements(args.db, diagnostics / "seed-pending-announcements.json")
    through = args.through or completed_china_date()
    if through > completed_china_date():
        raise ValueError("Requested through date has not completed in Asia/Shanghai")
    bootstrap = restore(args.inputs, work / "official-evidence")
    if bootstrap["through"] != prior.isoformat():
        raise ValueError("Official inputs and SQLite seed have different through dates")
    status = {"publish": False, "from": (prior + timedelta(days=1)).isoformat(), "through": through.isoformat()}
    if through <= prior:
        status["reason"] = "seed_already_current"
    else:
        evidence, raw = work / "official-evidence", work / "raw"
        days = refresh_calendar(evidence, raw, prior + timedelta(days=1), through)
        if not days:
            status["reason"] = "no_official_trading_days"
        else:
            # Persist the last observed trading day; a weekend must not extend
            # the summary beyond its evidence-backed daily observation range.
            through = date.fromisoformat(days[-1])
            status["through"] = through.isoformat()
            settle, sources, meta = work / "settle_params", work / "settlement-evidence", work / "metadata"
            meta.mkdir()
            run(sys.executable, ROOT / "scripts/collect_settle_params.py",
                "--start-date", status["from"], "--end-date", through,
                "--output-dir", settle, "--raw-dir", raw, "--evidence-dir", evidence,
                "--provenance-dir", sources, "--futures-only", "--forward-fill-cffex")
            common = [sys.executable, ROOT / "scripts/collect_contract_specs.py", "--evidence-dir", evidence,
                      "--settle-dir", settle, "--out", meta / "contracts_meta.csv",
                      "--versions-out", meta / "contract_spec_versions.csv"]
            # First acquire dated CBI originals; only the subsequent strict pass
            # may authorize a rebuilt delta. Candidate gaps remain diagnostic.
            run(*common, "--allow-incomplete", *[arg for day in days for arg in ("--cbi-date", day)])
            lifecycle = [sys.executable, ROOT / "scripts/build_lifecycles.py", "--evidence-dir", evidence, "--settle-dir", settle]
            assumed = bootstrap.get("lifecycle_options", {}).get("assume_2027_holidays", False)
            if not isinstance(assumed, bool):
                raise ValueError("Invalid explicit lifecycle assumption setting")
            if assumed:
                lifecycle.append("--assume-2027-holidays")
            existing_lifecycles = lifecycle_snapshot(evidence)
            run(*lifecycle)
            preserve_known_lifecycles(evidence, existing_lifecycles)
            run(*common, "--offline")
            delta, merged = work / "delta.sqlite", work / "merged.sqlite"
            daemon("rebuild-collected", "--metadata-dir", meta, "--settle-dir", settle,
                   "--settlement-sources", sources / "settlement_sources.json", "--out", delta)
            daemon("update-collected", "--db", args.db.resolve(), "--delta", delta, "--out", merged)
            try:
                daemon("scan-announcements", "--db", merged)
            except subprocess.CalledProcessError as error:
                # Broker notices are supplementary audit, not parameter sources.
                # Preserve pending state and make the audit outage explicit.
                (diagnostics / "announcement-audit-error.json").write_text(json.dumps({
                    "command": error.cmd, "exit_status": error.returncode,
                    "official_update_blocked": False}, indent=2))
                print("Supplementary announcement scan failed; pending candidates remain unresolved", flush=True)
            pending_announcements(merged, diagnostics / "pending-announcements.json")
            public.mkdir(parents=True, exist_ok=True)
            daemon("verify-collected", "--db", merged, "--out", public / "coverage.json")
            daemon("export", "--db", merged, "--out", public)
            compact = work / "compacted.sqlite"
            daemon("compact-seed", "--db", merged, "--out", compact)
            ops = public / "ops"
            ops.mkdir(exist_ok=True)
            import gzip
            with compact.open("rb") as source, gzip.open(ops / "future-meta.sqlite.gz", "wb", compresslevel=9) as target:
                shutil.copyfileobj(source, target)
            package(evidence, meta / "contracts_meta_provenance.json", through.isoformat(),
                    ops / "official-inputs.json", assumed)
            if (ops / "future-meta.sqlite.gz").stat().st_size > 25 * 1024 * 1024:
                raise ValueError("Seed exceeds Cloudflare Pages 25 MiB file limit")
            status["publish"] = True
    (diagnostics / "update-status.json").write_text(json.dumps(status, indent=2))
    if args.github_output:
        with args.github_output.open("a") as output:
            output.write(f"publish={str(status['publish']).lower()}\n")
    print(json.dumps(status), flush=True)
    return status


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", type=Path, required=True)
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True, help="Fresh run directory")
    parser.add_argument("--public-dir", type=Path, default=Path("public"))
    parser.add_argument("--through", type=date.fromisoformat)
    parser.add_argument("--github-output", type=Path)
    update(parser.parse_args())


if __name__ == "__main__":
    main()
