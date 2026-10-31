#!/usr/bin/env python3
"""Atomic, small publication snapshots in the orphan `data` branch."""

import base64
import copy
import json
import os
import re
import subprocess
from datetime import date, datetime

EXCHANGES = ("CFFEX", "CZCE", "DCE", "GFEX", "INE", "SHFE")
BRANCH = "data"
MANIFEST_URL = (
    "https://raw.githubusercontent.com/zynthium/future-meta/data/manifest.json"
)


def api(repository, endpoint, method="GET", body=None):
    command = ["gh", "api", "--method", method, f"repos/{repository}/{endpoint}"]
    if body is not None:
        command += ["--input", "-"]
    result = subprocess.run(
        command,
        input=None if body is None else json.dumps(body),
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(f"GitHub {method} {endpoint}: {result.stderr.strip()}")
    return json.loads(result.stdout) if result.stdout.strip() else None


def validate(manifest, state):
    if state["schema"] != 1 or manifest["schema_version"] != 4:
        raise ValueError("Unsupported publication schema")
    if (
        not re.fullmatch(r"[a-f0-9]{64}", manifest["sha256"])
        or manifest["sha256"] != state["archive_sha256"]
    ):
        raise ValueError("Manifest/checkpoint SHA mismatch")
    if (
        manifest["artifact"] != "latest.fmeta.zst"
        or not isinstance(manifest["size"], int)
        or manifest["size"] <= 0
    ):
        raise ValueError("Invalid client artifact metadata")
    checkpoints = state["checked_through"]
    if set(checkpoints) != set(EXCHANGES) or len(set(checkpoints.values())) != 1:
        raise ValueError("Six-exchange checkpoint is not atomic")
    for value in checkpoints.values():
        date.fromisoformat(value)
    pattern = r"data-\d{8}T\d{6}Z-[a-f0-9]{12}"
    if not re.fullmatch(pattern, state["current_release"]):
        raise ValueError("Invalid current data Release")
    previous = state.get("previous_release")
    if previous and (
        not re.fullmatch(pattern, previous) or previous == state["current_release"]
    ):
        raise ValueError("Invalid previous data Release")
    if len(manifest["mirrors"]) != 1 or not manifest["mirrors"][0].endswith(
        f"/releases/download/{state['current_release']}/latest.fmeta.zst"
    ):
        raise ValueError("Manifest and Release tag differ")


def read(repository):
    commit_sha = api(repository, "git/ref/heads/data")["object"]["sha"]
    documents = []
    for name in ("manifest.json", "update-state.json"):
        blob = api(repository, f"contents/{name}?ref={commit_sha}")
        if blob["encoding"] != "base64":
            raise ValueError("Expected a small JSON blob")
        documents.append(json.loads(base64.b64decode(blob["content"])))
    manifest, state = documents
    validate(manifest, state)
    return commit_sha, manifest, state


def commit(repository, parent, manifest, state, message):
    validate(manifest, state)
    if (
        parent is not None
        and api(repository, "git/ref/heads/data")["object"]["sha"] != parent
    ):
        raise ValueError("Data branch changed; refusing stale publication")
    entries = [
        {
            "path": name,
            "mode": "100644",
            "type": "blob",
            "content": json.dumps(document, ensure_ascii=False, indent=2) + "\n",
        }
        for name, document in [
            ("manifest.json", manifest),
            ("update-state.json", state),
        ]
    ]
    tree = api(repository, "git/trees", "POST", {"tree": entries})["sha"]
    body = {
        "message": message,
        "tree": tree,
        "parents": [] if parent is None else [parent],
    }
    if os.environ.get("GITHUB_ACTIONS") == "true":
        identity = {
            "name": "github-actions[bot]",
            "email": "41898282+github-actions[bot]@users.noreply.github.com",
        }
        body.update(author=identity, committer=identity)
    sha = api(repository, "git/commits", "POST", body)["sha"]
    if parent is None:
        api(repository, "git/refs", "POST", {"ref": "refs/heads/data", "sha": sha})
    else:
        api(repository, "git/refs/heads/data", "PATCH", {"sha": sha, "force": False})
    return sha


def begin_attempt(state, slot, started_at, run_url, run_id):
    if state.get("last_attempt", {}) and state["last_attempt"]["slot"] == slot:
        raise ValueError("Update slot already attempted")
    updated = copy.deepcopy(state)
    updated["last_attempt"] = {
        "slot": slot,
        "started_at": started_at,
        "finished_at": None,
        "status": "running",
        "run_url": run_url,
        "run_id": str(run_id),
    }
    return updated


def finish_attempt(state, status, finished_at, through=None, error=None):
    if status not in ("failed", "unchanged", "changed"):
        raise ValueError("Invalid update outcome")
    updated = copy.deepcopy(state)
    if (
        not updated.get("last_attempt")
        or updated["last_attempt"]["status"] != "running"
    ):
        raise ValueError("No active update attempt")
    updated["last_attempt"].update(status=status, finished_at=finished_at)
    if status == "failed":
        updated["last_attempt"]["error"] = str(error)[:1000]
    else:
        if through is None or any(
            through < old for old in updated["checked_through"].values()
        ):
            raise ValueError("Successful check cannot regress")
        date.fromisoformat(through)
        if datetime.fromisoformat(finished_at).tzinfo is None:
            raise ValueError("Check timestamp requires a timezone")
        updated["last_success_at"] = finished_at
        updated["checked_through"] = {exchange: through for exchange in EXCHANGES}
    return updated
