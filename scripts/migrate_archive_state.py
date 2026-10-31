#!/usr/bin/env python3
"""One-time LOCAL extraction from a verified, preserved official maintenance store."""

import argparse
import csv
import hashlib
import json
import sqlite3
import xml.etree.ElementTree as ET
from datetime import date, timedelta
from pathlib import Path

import data_branch as branch


def extract(backup, out, rules_out):
    backup, out, rules_out = Path(backup), Path(out), Path(rules_out)
    inventory = json.loads((backup / "inventory.json").read_text())
    for name, digest in inventory["files"].items():
        if hashlib.sha256((backup / name).read_bytes()).hexdigest() != digest:
            raise ValueError(f"Local backup checksum mismatch: {name}")
    manifest = json.loads((backup / "manifest.json").read_text())
    if (
        hashlib.sha256((backup / "latest.fmeta.zst").read_bytes()).hexdigest()
        != manifest["sha256"]
    ):
        raise ValueError("Client baseline SHA mismatch")
    evidence = backup / "official-evidence"
    db = backup / "future-meta.sqlite"
    with sqlite3.connect(f"file:{db.resolve()}?mode=ro", uri=True) as conn:
        summary = json.loads(
            conn.execute("select report_json from rebuild_summary").fetchone()[0]
        )
        through = summary["through"]
        exceptions = {
            "assumed_expiry": {},
            "assumptions": {},
            "without_observations": [],
        }
        for symbol, provenance in conn.execute(
            "select c.symbol,p.provenance_json from contracts c join rebuild_contract_provenance p on p.contract_id=c.id"
        ):
            data = json.loads(provenance)
            field = data.get("official_fields", {}).get("expiry_date", {})
            if field.get("calendar_assumption"):
                assumption = field["calendar_assumption"]
                key = (
                    "calendar-"
                    + hashlib.sha256(
                        json.dumps(assumption, sort_keys=True).encode()
                    ).hexdigest()[:12]
                )
                exceptions["assumptions"][key] = assumption
                exceptions["assumed_expiry"][symbol] = key
        exceptions["without_observations"] = [
            r[0]
            for r in conn.execute(
                "select symbol from contracts c where not exists(select 1 from official_observations o where o.contract_id=c.id) order by symbol"
            )
        ]
        snapshot = conn.execute(
            "select trading_date,canonical_url,body_sha256 from retained_parameter_documents where exchange='CFFEX' and trading_date<=? order by trading_date desc limit 1",
            (through,),
        ).fetchone()
    if summary["source"] != "fresh_collector_outputs_no_seed" or snapshot is None:
        raise ValueError("Migration requires an official, verified source baseline")
    state = {
        "schema": 1,
        "archive_sha256": manifest["sha256"],
        "checked_through": {e: through for e in branch.EXCHANGES},
        "last_attempt": None,
        "last_success_at": None,
        "current_release": manifest["mirrors"][0].split("/")[-2],
        "previous_release": None,
        "retired_releases": [],
        "exceptions": exceptions,
        "source_versions": {
            "cffex_snapshot": dict(zip(("date", "url", "sha256"), snapshot))
        },
    }
    branch.validate(manifest, state)
    fields = (
        "exchange",
        "product",
        "full_symbol",
        "valid_from",
        "valid_to",
        "lot_size",
        "tick_size",
        "valid_from_phase",
        "valid_to_phase",
        "valid_from_trade_date",
        "valid_to_trade_date",
        "delivery_month_from",
        "delivery_month_to",
    )
    specifications, sources = [], {}
    for path in sorted(evidence.glob("*product*tsv")):
        for row in csv.DictReader(path.open(), delimiter="\t"):
            exchange = row.get("exchange") or (
                "CFFEX" if path.name.startswith("cffex-") else ""
            )
            if row["product"] in ("IO", "HO", "MO") or (
                row.get("valid_to") and row["valid_to"] < through
            ):
                continue
            rule = {key: row.get(key, "") for key in fields}
            rule.update(
                exchange=exchange,
                lot_size=float(row["lot_size"]),
                tick_size=float(row["tick_size"]),
            )
            url = row.get("canonical_url") or row["specification_url"]
            sha = row.get("sha256") or row["specification_sha256"]
            if url in sources and sources[url]["sha256"] != sha:
                raise ValueError(f"Conflicting current official source: {url}")
            sources[url] = {"exchange": exchange, "url": url, "sha256": sha}
            rule["source_url"], rule["source_sha256"] = url, sha
            specifications.append(rule)
    calendars = {}
    seen = set()
    for path in sorted(evidence.glob("cffex-calendar-*.tsv")):
        for row in csv.DictReader(path.open(), delimiter="\t"):
            if row["sha256"] in seen:
                continue
            seen.add(row["sha256"])
            month = row["month"]
            cal = calendars.setdefault(
                month[:4], {"closed_weekdays": [], "assumption": None}
            )
            for doc in ET.fromstring(
                (evidence / (row["sha256"] + ".xml")).read_bytes()
            ).findall(".//doc"):
                title = doc.findtext("title") or ""
                if any(word in title for word in ("休市", "节", "假")):
                    cal["closed_weekdays"].append(doc.findtext("pubdate").strip())
    # Preserve the already approved lifecycle-only assumption. It is never used
    # as the admission calendar for collecting 2027 observations.
    from build_lifecycles import ASSUMED_2027_CLOSURES, CALENDAR_ASSUMPTION

    closures = []
    for first, last in ASSUMED_2027_CLOSURES:
        current = date.fromisoformat(first)
        while current <= date.fromisoformat(last):
            closures.append(current.isoformat())
            current += timedelta(days=1)
    calendars["2027"] = {
        "closed_weekdays": closures,
        "assumption": json.loads(CALENDAR_ASSUMPTION),
    }
    for calendar in calendars.values():
        calendar["closed_weekdays"] = sorted(set(calendar["closed_weekdays"]))
    rules = {
        "schema": 1,
        "approved_through": through,
        "specifications": specifications,
        "sources": list(sources.values()),
        "lifecycle_calendars": calendars,
    }
    out.mkdir(parents=True, exist_ok=False)
    for name, document in [("manifest.json", manifest), ("update-state.json", state)]:
        (out / name).write_text(
            json.dumps(document, ensure_ascii=False, indent=2) + "\n"
        )
    from catalog_guard import extract_monitors

    rules["catalogs"] = extract_monitors(evidence, rules)
    rules_out.write_text(json.dumps(rules, ensure_ascii=False, indent=2) + "\n")
    print(
        json.dumps(
            {
                "through": through,
                "rules": len(specifications),
                "sources": len(sources),
                "state_bytes": (out / "update-state.json").stat().st_size,
                "rules_bytes": rules_out.stat().st_size,
            }
        )
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backup", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--rules-out", type=Path, required=True)
    args = parser.parse_args()
    extract(args.backup, args.out, args.rules_out)
