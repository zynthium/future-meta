#!/usr/bin/env python3
"""Explicit rule/document audit, separate from daily parameter updates."""

import argparse
import hashlib
import json
import os
import time
from datetime import datetime, timezone
from pathlib import Path

import official_access
from update_schedule import access_deadline, decide

ROOT = Path(__file__).resolve().parents[1]
AUDITS = ROOT / "data/rule-audits"


def fetch_source(source, previous):
    headers = {"User-Agent": "future-meta-official-update"}
    if previous.get("etag"):
        headers["If-None-Match"] = previous["etag"]
    elif previous.get("last_modified"):
        headers["If-Modified-Since"] = previous["last_modified"]
    for attempt in range(3):
        try:
            response = official_access.request(
                "GET", source["url"], headers=headers, timeout=15
            )
            if response.status_code in (200, 304):
                return response
            raise RuntimeError(
                f"Official rule HTTP {response.status_code}: {source['url']}"
            )
        except official_access.AccessWindowClosed:
            raise
        except (OSError, RuntimeError) as error:
            if attempt == 2:
                raise RuntimeError(
                    f"Official rule unavailable: {source['url']}"
                ) from error
            time.sleep((2, 8)[attempt])


def check_rule_sources(rules, previous):
    versions = {}
    for source in rules["sources"]:
        old = previous.get(source["url"], {})
        # Validators belong to the exact approved digest, not just the URL.
        response = fetch_source(
            source, old if old.get("sha256") == source["sha256"] else {}
        )
        if response.status_code == 304:
            if old.get("sha256") != source["sha256"]:
                raise ValueError("Unbound official source validator")
            versions[source["url"]] = dict(old)
        else:
            if hashlib.sha256(response.content).hexdigest() != source["sha256"]:
                raise ValueError(
                    f"Official rule changed; local review required: {source['url']}"
                )
            versions[source["url"]] = {
                "sha256": source["sha256"],
                "etag": response.headers.get("ETag"),
                "last_modified": response.headers.get("Last-Modified"),
            }
    return versions


def utcnow():
    return datetime.now(timezone.utc).isoformat()


def audit(args):
    gate = decide()
    if not gate["should_update"]:
        return {"status": "skipped", "reason": gate["reason"]}
    rules = json.loads(args.rules.read_text())
    if rules["schema"] != 1 or not rules.get("sources"):
        raise ValueError("Missing approved official rules")
    previous = json.loads(args.previous.read_text()) if args.previous else {}
    if previous and previous.get("status") != "success":
        raise ValueError("Previous rule audit must be successful")
    slot = f"{datetime.fromisoformat(gate['evaluated_at']).date()}-{gate['slot']}"
    work_dir = AUDITS / slot
    # The fixed slot directory is both a local report and an atomic attempt
    # claim. Keep it after failure so retries cannot start a second full audit.
    try:
        work_dir.mkdir(parents=True, exist_ok=False)
    except FileExistsError:
        return {"status": "skipped", "reason": "slot_already_attempted"}
    report = {
        "schema": 1,
        "slot": slot,
        "status": "running",
        "started_at": utcnow(),
        "finished_at": None,
        "rules_sha256": hashlib.sha256(
            json.dumps(rules, sort_keys=True).encode()
        ).hexdigest(),
    }
    path = work_dir / "rule-audit.json"
    path.write_text(json.dumps(report, indent=2) + "\n")
    prior_deadline = os.environ.get("FUTURE_META_ACCESS_DEADLINE")
    os.environ["FUTURE_META_ACCESS_DEADLINE"] = access_deadline(gate)
    try:
        import catalog_guard

        versions = check_rule_sources(rules, previous.get("rules", {}))
        catalogs = catalog_guard.check(rules, versions, previous.get("catalogs", {}))
        report.update(status="success", rules=versions, catalogs=catalogs)
    except Exception as error:
        report.update(status="failed", error=type(error).__name__ + ": " + str(error))
        raise
    finally:
        if prior_deadline is None:
            os.environ.pop("FUTURE_META_ACCESS_DEADLINE", None)
        else:
            os.environ["FUTURE_META_ACCESS_DEADLINE"] = prior_deadline
        report["finished_at"] = utcnow()
        path.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--rules", type=Path, default=ROOT / "config/official-rules.json"
    )
    parser.add_argument(
        "--previous", type=Path, help="Previous successful local audit report"
    )
    print(json.dumps(audit(parser.parse_args())))


if __name__ == "__main__":
    main()
