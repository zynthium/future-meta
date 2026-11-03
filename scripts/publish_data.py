#!/usr/bin/env python3
"""Verify immutable data Releases, then atomically publish small branch metadata."""

import hashlib
import json
import re
import subprocess
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from urllib.request import Request, urlopen

import data_branch as branch

MARKER = "<!-- future-meta:data-release -->"
TAG_PATTERN = r"data-\d{8}T\d{6}Z-[0-9a-f]{12}"
USER_AGENT = "future-meta-publisher (+https://github.com/zynthium/future-meta)"


def gh(*args, check=True):
    result = subprocess.run(["gh", *args], capture_output=True, text=True)
    if check and result.returncode:
        raise RuntimeError(
            f"GitHub command failed ({' '.join(args[:3])}): {result.stderr.strip()}"
        )
    return result


def download_asset(url, limit):
    with urlopen(
        Request(url, headers={"User-Agent": USER_AGENT, "Cache-Control": "no-cache"}),
        timeout=60,
    ) as response:
        return response.read(limit)


def release_tag(manifest):
    generated = datetime.fromisoformat(manifest["generated_at"])
    if generated.tzinfo is None:
        raise ValueError("Archive generation time requires a timezone")
    stamp = generated.astimezone(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return f"data-{stamp}-{manifest['sha256'][:12]}"


def asset_url(repository, tag, artifact):
    return f"https://github.com/{repository}/releases/download/{tag}/{artifact}"


def release_info(repository, tag):
    result = gh(
        "release",
        "view",
        tag,
        "--repo",
        repository,
        "--json",
        "tagName,isDraft,body",
        check=False,
    )
    if result.returncode:
        if "release not found" in result.stderr.lower():
            return None
        raise RuntimeError(result.stderr.strip())
    return json.loads(result.stdout)


def ensure_release(repository, target, tag, artifact, manifest):
    release = release_info(repository, tag)
    if release and not (release.get("body") or "").startswith(MARKER):
        raise ValueError("Refusing to modify an unmanaged Release")
    if release and release["isDraft"]:
        # Only an unpublished, owned draft can be replaced after failed upload.
        gh("release", "delete", tag, "--repo", repository, "--cleanup-tag", "--yes")
        release = None
    if release is None:
        with tempfile.TemporaryDirectory(prefix="future-meta-release-") as temp:
            notes = Path(temp) / "notes.md"
            notes.write_text(
                f"{MARKER}\nClient data {manifest['data_version']}\n\nSHA-256: {manifest['sha256']}\n"
            )
            gh(
                "release",
                "create",
                tag,
                str(artifact),
                "--repo",
                repository,
                "--target",
                target,
                "--draft",
                "--latest=false",
                "--title",
                f"Fee data {manifest['data_version']}",
                "--notes-file",
                str(notes),
            )
        gh(
            "release",
            "edit",
            tag,
            "--repo",
            repository,
            "--draft=false",
            "--latest=false",
        )
    url = asset_url(repository, tag, manifest["artifact"])
    body = download_asset(url, manifest["size"] + 1)
    if (
        len(body) != manifest["size"]
        or hashlib.sha256(body).hexdigest() != manifest["sha256"]
    ):
        raise ValueError("Published Release asset checksum mismatch")
    return url


def staged_archive(source):
    source = Path(source)
    manifest = json.loads((source / "manifest.json").read_text())
    if manifest["artifact"] != "latest.fmeta.zst":
        raise ValueError("Invalid client artifact filename")
    artifact = source / "latest.fmeta.zst"
    body = artifact.read_bytes()
    if (
        len(body) != manifest["size"]
        or hashlib.sha256(body).hexdigest() != manifest["sha256"]
    ):
        raise ValueError("Client archive checksum mismatch")
    return manifest, artifact


def publish(source, repository, target, parent, old_manifest, old_state, state):
    # No-change publication preserves every client manifest field and artifact byte.
    manifest = dict(old_manifest)
    state = dict(state)
    if source is not None:
        incoming, artifact = staged_archive(source)
        if incoming["sha256"] != old_manifest["sha256"]:
            tag = release_tag(incoming)
            url = ensure_release(repository, target, tag, artifact, incoming)
            incoming["mirrors"] = [url]
            manifest = incoming
            retired = set(state.get("retired_releases", []))
            if old_state.get("previous_release"):
                retired.add(old_state["previous_release"])
            state["retired_releases"] = sorted(retired)
            state.update(
                archive_sha256=incoming["sha256"],
                current_release=tag,
                previous_release=old_state["current_release"],
            )
    branch.validate(manifest, state)
    return branch.commit(
        repository, parent, manifest, state, "data: record official update outcome"
    )


def verify_live(repository, expected_commit, manifest, state):
    current, actual_manifest, actual_state = branch.read(repository)
    if (current, actual_manifest, actual_state) != (expected_commit, manifest, state):
        raise ValueError("Canonical branch changed; retaining Releases")
    base = f"https://raw.githubusercontent.com/{repository}/data/"
    last_error = None
    # Raw CDN propagation is independent of the successful atomic Git commit.
    delays = (5, 10, 20, 30, 60, 60, 60, 60)
    for attempt in range(len(delays) + 1):
        try:
            actual = [
                json.loads(download_asset(base + name, 2_000_000))
                for name in ("manifest.json", "update-state.json")
            ]
            if actual == [manifest, state]:
                return
            last_error = ValueError("Canonical raw metadata has not propagated")
        except (OSError, ValueError) as error:
            last_error = error
        if attempt < len(delays):
            time.sleep(delays[attempt])
    raise ValueError(f"Public readback failed: {last_error}")


def cleanup(repository, expected_commit, manifest, state):
    verify_live(repository, expected_commit, manifest, state)
    keep = {state["current_release"], state["previous_release"]}
    releases = json.loads(
        gh(
            "api", "--paginate", "--slurp", f"repos/{repository}/releases?per_page=100"
        ).stdout
    )
    retired = set(state.get("retired_releases", []))
    deleted = []
    for release in (item for page in releases for item in page):
        tag = release["tag_name"]
        if (
            re.fullmatch(TAG_PATTERN, tag)
            and (release.get("body") or "").startswith(MARKER)
            and tag not in keep
            and tag in retired
        ):
            gh("release", "delete", tag, "--repo", repository, "--cleanup-tag", "--yes")
            deleted.append(tag)
    return {"kept": sorted(tag for tag in keep if tag), "deleted": deleted}
