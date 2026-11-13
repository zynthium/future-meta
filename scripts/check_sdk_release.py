"""Validate a stable SDK tag and select the previous SDK tag for release notes."""

import argparse
import json
from pathlib import Path
import re
import subprocess
import tomllib


SDK_TAG = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args], text=True).strip()


def validate_release(repo, tag, main_ref="origin/main"):
    match = SDK_TAG.fullmatch(tag)
    if match is None:
        raise ValueError("SDK tag must be a stable vX.Y.Z version without leading zeroes")
    version = tag[1:]
    package = tomllib.loads((repo / "crates/future-meta/Cargo.toml").read_text())["package"]
    if package["version"] != version:
        raise ValueError("SDK tag does not match crates/future-meta/Cargo.toml")
    lock = tomllib.loads((repo / "Cargo.lock").read_text())
    packages = [p for p in lock["package"] if p["name"] == "future-meta"]
    if len(packages) != 1 or packages[0]["version"] != version:
        raise ValueError("SDK tag does not match Cargo.lock")
    py_manifest = repo / "crates/future-meta-py/Cargo.toml"
    if py_manifest.exists():
        py_package = tomllib.loads(py_manifest.read_text())["package"]
        if py_package["version"] != version:
            raise ValueError("SDK tag does not match crates/future-meta-py/Cargo.toml")
        py_packages = [p for p in lock["package"] if p["name"] == "future-meta-py"]
        if len(py_packages) != 1 or py_packages[0]["version"] != version:
            raise ValueError("SDK tag does not match future-meta-py in Cargo.lock")
    if git(repo, "rev-parse", f"refs/tags/{tag}^{{commit}}") != git(repo, "rev-parse", "HEAD"):
        raise ValueError("SDK tag does not point to the checked-out commit")
    ancestor = subprocess.run(
        ["git", "-C", str(repo), "merge-base", "--is-ancestor", "HEAD", main_ref],
        capture_output=True,
    )
    if ancestor.returncode == 1:
        raise ValueError("SDK tag commit must already belong to main")
    ancestor.check_returncode()
    previous = []
    for name in git(repo, "tag", "--merged", "HEAD", "--list", "v*").splitlines():
        parsed = SDK_TAG.fullmatch(name)
        if parsed is not None and name != tag:
            previous.append((tuple(map(int, parsed.groups())), name))
    latest = max(previous, default=None)
    if latest is not None and latest[0] >= tuple(map(int, match.groups())):
        raise ValueError("SDK version must increase from the previous SDK tag")
    return {"version": version, "previous_tag": latest[1] if latest else ""}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args()
    try:
        result = validate_release(Path.cwd(), args.tag)
    except (ValueError, subprocess.CalledProcessError) as error:
        parser.error(str(error))
    if args.github_output:
        with args.github_output.open("a") as output:
            for key, value in result.items():
                output.write(f"{key}={value}\n")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
