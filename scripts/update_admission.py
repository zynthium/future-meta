#!/usr/bin/env python3
"""Read-only GitHub admission and freshness checks; never contact exchanges."""

import argparse
from datetime import date
import json
import os
from pathlib import Path
import subprocess

import ci_scope
import data_branch as branch
import update_schedule as schedule

ROOT = Path(__file__).resolve().parents[1]


def verified_ci(repository):
    """Require successful full CI for the same executable inputs as HEAD.

    A documentation-only push can reuse an earlier full check. Its own green
    documentation job cannot conceal a failed or unchecked code change.
    """
    runs = branch.api(
        repository,
        "actions/workflows/ci.yml/runs?branch=main&event=push&status=success&per_page=100",
    )["workflow_runs"]
    for run in runs:
        sha = run["head_sha"]
        ancestor = subprocess.run(
            ["git", "merge-base", "--is-ancestor", sha, "HEAD"],
            cwd=ROOT,
            capture_output=True,
            check=False,
        )
        if ancestor.returncode == 1:
            continue
        if ancestor.returncode != 0:
            raise RuntimeError("Cannot verify CI commit ancestry")
        if not ci_scope.documentation_only(ci_scope.changed_paths(ROOT, sha)):
            continue
        jobs = branch.api(repository, f"actions/runs/{run['id']}/jobs?per_page=100")[
            "jobs"
        ]
        if any(
            job["name"] == "code" and job["conclusion"] == "success" for job in jobs
        ):
            return run["html_url"]
    return None


def checkpoint(state):
    # branch.read already validates the six-exchange atomic checkpoint.
    return next(iter(state["checked_through"].values()))


def verify_through(repository, target):
    if date.fromisoformat(target).isoformat() != target:
        raise ValueError("Checkpoint target must use YYYY-MM-DD")
    commit, _, state = branch.read(repository)
    checked = checkpoint(state)
    return {
        "should_update": False,
        "ok": checked >= target,
        "reason": "checkpoint_verified" if checked >= target else "missed_update",
        "target_through": target,
        "checked_through": checked,
        "data_commit": commit,
        "last_attempt": state.get("last_attempt"),
    }


def assess(repository, now=None, health_only=False):
    now = schedule.china_time(now)
    gate = schedule.decide(now)
    result = {**gate, "should_update": False, "ok": True}
    # Weekends do not even read GitHub state or require calendar coverage.
    if gate["reason"] == "weekend":
        return result
    commit, _, state = branch.read(repository)
    checked = checkpoint(state)
    result.update(checked_through=checked, data_commit=commit)
    if gate["should_update"] and not health_only:
        target = schedule.collection_target(now)
        slot = f"{now.date().isoformat()}:{gate['slot']}"
        result.update(target_through=target, slot=slot)
        if checked >= target:
            return {**result, "reason": "already_checked"}
        attempt = state.get("last_attempt")
        if attempt and attempt["slot"] == slot:
            return {
                **result,
                "ok": False,
                "reason": "slot_already_attempted",
                "last_attempt": attempt,
            }
        ci_url = verified_ci(repository)
        if not ci_url:
            return {**result, "ok": False, "reason": "ci_not_verified"}
        return {
            **result,
            "should_update": True,
            "reason": "session_due",
            "ci_url": ci_url,
        }

    expected = schedule.latest_completed_slot(now)
    result.update(expected)
    if checked < expected["target_through"]:
        return {
            **result,
            "ok": False,
            "reason": "missed_update",
            "last_attempt": state.get("last_attempt"),
        }
    return {**result, "reason": "checkpoint_verified"}


def emit(result, github_output=None):
    if github_output:
        with github_output.open("a") as output:
            output.write(f"should_update={str(result['should_update']).lower()}\n")
            output.write(f"target_through={result.get('target_through', '')}\n")
    encoded = json.dumps(result, ensure_ascii=False, indent=2)
    print(encoded)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with Path(summary).open("a") as report:
            report.write(
                "### Official update admission / checkpoint\n\n```json\n"
                + encoded
                + "\n```\n"
            )
    if not result["ok"]:
        print(
            f"::error::Official update blocked: {result['reason']}; "
            f"checked={result.get('checked_through')}, required={result.get('target_through')}"
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default="zynthium/future-meta")
    parser.add_argument("--github-output", type=Path)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--verify-through", help="Only verify the explicit admitted target"
    )
    mode.add_argument("--republish-current", action="store_true")
    mode.add_argument("--health-only", action="store_true")
    args = parser.parse_args()
    if args.republish_current:
        result = {
            "should_update": False,
            "ok": True,
            "reason": "explicit_publication_verification",
        }
    elif args.verify_through:
        result = verify_through(args.repo, args.verify_through)
    else:
        result = assess(args.repo, health_only=args.health_only)
    emit(result, args.github_output)
    if not result["ok"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
