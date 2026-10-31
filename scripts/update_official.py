#!/usr/bin/env python3
"""Official missing-day updates from a client archive and a small Git checkpoint."""

import argparse
import hashlib
import json
import os
import subprocess
import sys
from datetime import datetime, time, timedelta, timezone
from pathlib import Path
from zoneinfo import ZoneInfo

import data_branch as branch
import publish_data
from update_schedule import decide as scheduled_decision

ROOT = Path(__file__).resolve().parents[1]


def completed_china_date(now=None):
    now = now or datetime.now(ZoneInfo("Asia/Shanghai"))
    return now.date() if now.time() >= time(18, 30) else now.date() - timedelta(days=1)


def utcnow():
    return datetime.now(timezone.utc).isoformat()


def daemon(*args):
    command = [
        "cargo",
        "run",
        "-q",
        "-p",
        "future-meta-daemon",
        "--",
        *[str(a) for a in args],
    ]
    subprocess.run(command, cwd=ROOT, check=True)


def restore_archive(manifest, work):
    path = work / "baseline.fmeta.zst"
    body = publish_data.download_asset(manifest["mirrors"][0], manifest["size"] + 1)
    if (
        len(body) != manifest["size"]
        or hashlib.sha256(body).hexdigest() != manifest["sha256"]
    ):
        raise ValueError("Published archive checksum mismatch")
    path.write_bytes(body)
    daemon("inspect-archive", "--archive", path, "--out", work / "baseline.json")
    return path, json.loads((work / "baseline.json").read_text())


def cleanup_publication(repository, parent, manifest, state):
    publish_data.cleanup(repository, parent, manifest, state)
    if state.get("retired_releases"):
        state = dict(state, retired_releases=[])
        parent = branch.commit(
            repository,
            parent,
            manifest,
            state,
            "data: finish verified Release retention",
        )
        publish_data.verify_live(repository, parent, manifest, state)
    return parent


def update(args):
    republish = args.republish_current
    gate = None if republish else scheduled_decision()
    if gate is not None and not gate["should_update"]:
        return {"publish": False, "reason": gate["reason"]}
    parent, manifest, old_state = branch.read(args.repo)
    if republish:
        args.work_dir.mkdir(parents=True, exist_ok=False)
        restore_archive(manifest, args.work_dir)
        parent = branch.commit(
            args.repo, parent, manifest, old_state, "data: verify publication access"
        )
        publish_data.verify_live(args.repo, parent, manifest, old_state)
        parent = cleanup_publication(args.repo, parent, manifest, old_state)
        return {
            "publish": False,
            "reason": "current_publication_verified",
            "commit": parent,
        }
    evaluated = datetime.fromisoformat(gate["evaluated_at"])
    slot = f"{evaluated.date().isoformat()}:{gate['slot']}"
    if old_state.get("last_attempt") and old_state["last_attempt"]["slot"] == slot:
        return {"publish": False, "reason": "slot_already_attempted"}
    from incremental_inputs import collect_interval, trading_dates

    prior = next(iter(old_state["checked_through"].values()))
    start = (datetime.fromisoformat(prior).date() + timedelta(days=1)).isoformat()
    days = trading_dates(start, completed_china_date(evaluated).isoformat())
    if not days:
        return {"publish": False, "reason": "already_checked"}
    run_id = os.environ.get("GITHUB_RUN_ID", "local-" + utcnow())
    run_url = (
        f"https://github.com/{args.repo}/actions/runs/{run_id}"
        if "GITHUB_RUN_ID" in os.environ
        else None
    )
    started = branch.begin_attempt(old_state, slot, utcnow(), run_url, run_id)
    parent = branch.commit(
        args.repo, parent, manifest, started, "data: begin official check"
    )
    # Only official transports read this deadline; GitHub publication may finish
    # after the collection window without contacting an exchange again.
    deadline = evaluated.replace(
        hour=20 if gate["slot"] == "evening" else 8,
        minute=0 if gate["slot"] == "evening" else 20,
        second=0,
        microsecond=0,
    )
    previous_deadline = os.environ.get("FUTURE_META_ACCESS_DEADLINE")
    os.environ["FUTURE_META_ACCESS_DEADLINE"] = deadline.isoformat()
    published = False
    try:
        args.work_dir.mkdir(parents=True, exist_ok=False)
        archive_path, archive = restore_archive(manifest, args.work_dir)
        rules = json.loads(args.rules.read_text())
        if rules["schema"] != 1:
            raise ValueError("Unsupported approved rules schema")
        snapshots, versions, exceptions = collect_interval(
            archive, started, rules, days, args.work_dir / "collected"
        )
        batch = {
            "baseline_sha256": manifest["sha256"],
            "previous_through": prior,
            "trading_days": days,
            "generated_at": started["last_attempt"]["started_at"],
            "snapshots": snapshots,
        }
        batch_path = args.work_dir / "batch.json"
        batch_path.write_text(json.dumps(batch))
        source = args.work_dir / "publication"
        daemon(
            "update-archive",
            "--archive",
            archive_path,
            "--input",
            batch_path,
            "--out",
            source,
        )
        report = json.loads((source / "update-report.json").read_text())
        state = branch.finish_attempt(
            started,
            "changed" if report["changed"] else "unchanged",
            utcnow(),
            through=days[-1],
        )
        state.update(source_versions=versions, exceptions=exceptions)
        new_commit = publish_data.publish(
            source if report["changed"] else None,
            args.repo,
            os.environ.get("GITHUB_SHA", "main"),
            parent,
            manifest,
            started,
            state,
        )
        published = True
        actual_commit, actual_manifest, actual_state = branch.read(args.repo)
        if actual_commit != new_commit:
            raise ValueError("Data branch changed during public verification")
        new_commit = cleanup_publication(
            args.repo, new_commit, actual_manifest, actual_state
        )
        return {
            "publish": bool(report["changed"]),
            "reason": "changed" if report["changed"] else "unchanged",
            "commit": new_commit,
            "checked_through": days[-1],
        }
    except Exception as error:
        if not published:
            failed = branch.finish_attempt(
                started,
                "failed",
                utcnow(),
                error=type(error).__name__ + ": " + str(error),
            )
            try:
                branch.commit(
                    args.repo,
                    parent,
                    manifest,
                    failed,
                    "data: record failed official check",
                )
            except Exception as status_error:
                print(
                    f"Failed to finalize check status; inspect Actions run: {status_error}",
                    file=sys.stderr,
                )
        raise
    finally:
        if previous_deadline is None:
            os.environ.pop("FUTURE_META_ACCESS_DEADLINE", None)
        else:
            os.environ["FUTURE_META_ACCESS_DEADLINE"] = previous_deadline


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default="zynthium/future-meta")
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument(
        "--rules", type=Path, default=ROOT / "config/official-rules.json"
    )
    parser.add_argument(
        "--republish-current",
        action="store_true",
        help="Verify current public state without exchange access",
    )
    args = parser.parse_args()
    print(json.dumps(update(args)), flush=True)


if __name__ == "__main__":
    main()
