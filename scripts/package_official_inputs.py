#!/usr/bin/env python3
"""Build/restore a portable, SHA-verified official metadata bootstrap (no SQLite)."""
import argparse
import csv
import hashlib
import io
import json
import re
import tarfile
from pathlib import Path
from urllib.parse import urljoin, urlparse
from urllib.request import Request, urlopen

USER_AGENT = "future-meta-daemon (+https://github.com/zynthium/future-meta)"
MANIFEST_PATTERNS = ("*-product-spec-*.tsv", "cffex-product-metadata.tsv",
                     "*-contract-base-info-*.tsv", "*-lifecycle-*.tsv", "cffex-calendar-*.tsv")
GFEX_CALENDAR = "f58f1b90262132b875cb47fa52163b044b87e63b15bfd78c401169c734685a26.json"


def digest(body):
    return hashlib.sha256(body).hexdigest()


def checked_raw(root, value, sha):
    if not re.fullmatch(r"[0-9a-f]{64}", sha):
        raise ValueError(f"Invalid evidence digest: {sha}")
    candidates = ([Path(value)] if value and Path(value).is_absolute() else
                  [root / value] if value else sorted(root.glob(sha + ".*")))
    for path in candidates:
        if path.is_file() and digest(path.read_bytes()) == sha:
            return path
    raise ValueError(f"Missing or corrupt official raw: {sha} ({value})")


def package(evidence_dir, provenance, through, out, assume_2027_holidays=False):
    root = Path(evidence_dir)
    files = {}
    validated = {}

    def resolve(value, sha):
        key = (value, sha)
        if key not in validated:
            validated[key] = checked_raw(root, value, sha)
        return validated[key]

    def retain(path, sha):
        name = sha + path.suffix
        if name in files:
            return name
        body = path.read_bytes()
        if digest(body) != sha:
            raise ValueError(f"Official raw hash mismatch: {path}")
        name = sha + path.suffix
        files[name] = body
        return name

    manifests = sorted({p for pattern in MANIFEST_PATTERNS for p in root.glob(pattern)})
    if not manifests:
        raise ValueError("No official metadata manifests")
    for path in manifests:
        with path.open(encoding="utf-8-sig", newline="") as handle:
            reader = csv.DictReader(handle, delimiter="\t")
            columns, rows = list(reader.fieldnames or []), list(reader)
        for row in rows:
            for column in list(row):
                if column != "sha256" and not column.endswith("_sha256"):
                    continue
                sha = row[column].strip()
                if not sha:
                    continue
                prefix = column[:-6] if column.endswith("sha256") else ""
                raw_column = prefix + "raw_path"
                raw = resolve(row.get(raw_column, ""), sha)
                portable = retain(raw, sha)
                if raw_column not in columns:
                    columns.append(raw_column)
                row[raw_column] = portable
        output = io.StringIO(newline="")
        writer = csv.DictWriter(output, columns, delimiter="\t", lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)
        files[path.name] = output.getvalue().encode()
    # Retain every document validated by the last metadata build, including CBI
    # cache entries outside the curated TSVs. Rebuild a portable CBI index below.
    if provenance:
        report = json.loads(Path(provenance).read_text())
        for entry in report["evidence"].values():
            retain(resolve(entry["raw_path"], entry["sha256"]), entry["sha256"])
    calendar = root / GFEX_CALENDAR
    retain(calendar, GFEX_CALENDAR.split(".")[0])
    # Network CBI cache is deliberately outside evidence_dir. Include its index
    # and raw bodies so the next run retains earlier dated observations.
    for cache in (root / "shfe-contract-base-info-incremental.tsv", root / "cbi-index.tsv", Path(provenance).parent / "official-cache" / "cbi-index.tsv" if provenance else root / "cbi-index.tsv"):
        if not cache.exists():
            continue
        with cache.open(newline="", encoding="utf-8") as handle:
            reader = csv.DictReader(handle, delimiter="\t")
            columns, rows = reader.fieldnames, list(reader)
        for row in rows:
            path = Path(row["raw_path"])
            if not path.is_absolute():
                path = cache.parent / path
            row["raw_path"] = retain(path, row["sha256"])
        output = io.StringIO(newline="")
        writer = csv.DictWriter(output, columns, delimiter="\t", lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)
        # Name matches collect_contract_specs.py's default CBI discovery pattern.
        key = "shfe-contract-base-info-incremental.tsv"
        previous = list(csv.DictReader(io.StringIO(files[key].decode()), delimiter="\t")) if key in files else []
        merged = {(r["exchange"], r["report_date"], r["sha256"]): r for r in previous + rows}
        output = io.StringIO(newline="")
        writer = csv.DictWriter(output, columns, delimiter="\t", lineterminator="\n")
        writer.writeheader()
        writer.writerows(merged[k] for k in sorted(merged))
        files[key] = output.getvalue().encode()
    manifest = {"schema": 1, "through": through,
                "lifecycle_options": {"assume_2027_holidays": assume_2027_holidays},
                "files": {name: digest(body) for name, body in sorted(files.items())}}
    out = Path(out)
    out.parent.mkdir(parents=True, exist_ok=True)
    archive_path = out.with_suffix(".tar.gz")
    with tarfile.open(archive_path, "w:gz", compresslevel=9) as archive:
        for name, body in [("bootstrap.json", json.dumps(manifest, sort_keys=True).encode()), *sorted(files.items())]:
            info = tarfile.TarInfo(name)
            info.size = len(body)
            info.mode = 0o644
            archive.addfile(info, io.BytesIO(body))
    with archive_path.open("rb") as source:
        sha = hashlib.file_digest(source, "sha256").hexdigest()
    transport = {"schema": 2, "through": through, "file": archive_path.name,
                 "sha256": sha, "size": archive_path.stat().st_size}
    out.write_text(json.dumps(transport, sort_keys=True, indent=2))
    return manifest


def transport_manifest(path):
    transport = json.loads(Path(path).read_text())
    if transport.get("schema") != 2:
        raise ValueError("Invalid official inputs transport manifest")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*\.tar\.gz", transport.get("file", "")):
        raise ValueError("Unsafe bootstrap archive filename")
    if not re.fullmatch(r"[0-9a-f]{64}", transport.get("sha256", "")) or type(transport.get("size")) is not int or transport["size"] <= 0:
        raise ValueError("Invalid bootstrap archive size/hash")
    return transport


def verified_archive(manifest_path):
    manifest_path = Path(manifest_path)
    transport = transport_manifest(manifest_path)
    archive_path = manifest_path.parent / transport["file"]
    with archive_path.open("rb") as source:
        sha = hashlib.file_digest(source, "sha256").hexdigest()
    if archive_path.stat().st_size != transport["size"] or sha != transport["sha256"]:
        raise ValueError("Bootstrap archive checksum mismatch")
    return transport, archive_path


def fetch(url, out):
    """Download one verified archive beside its GitHub Pages ops manifest."""
    parsed = urlparse(url)
    if parsed.scheme != "https" or parsed.username or parsed.password:
        raise ValueError("Official bootstrap URL must use HTTPS without credentials")
    out = Path(out)
    out.parent.mkdir(parents=True, exist_ok=True)
    with urlopen(Request(url, headers={"User-Agent": USER_AGENT}), timeout=60) as response:
        out.write_bytes(response.read())
    transport = transport_manifest(out)
    archive_url = urljoin(url, transport["file"])
    with urlopen(Request(archive_url, headers={"User-Agent": USER_AGENT}), timeout=120) as response:
        body = response.read(transport["size"] + 1)
    if len(body) != transport["size"] or digest(body) != transport["sha256"]:
        raise ValueError("Downloaded bootstrap archive checksum mismatch")
    (out.parent / transport["file"]).write_bytes(body)
    return transport


def restore(manifest_path, destination):
    transport, archive_path = verified_archive(manifest_path)
    destination = Path(destination)
    if destination.exists() and any(destination.iterdir()):
        raise FileExistsError(f"Bootstrap destination must be empty: {destination}")
    with tarfile.open(archive_path, "r:gz") as archive:
        members = archive.getmembers()
        names = [m.name for m in members]
        if len(names) != len(set(names)) or any(not m.isfile() or Path(m.name).name != m.name for m in members):
            raise ValueError("Unsafe or duplicate bootstrap entry")
        if "bootstrap.json" not in names:
            raise ValueError("Missing bootstrap manifest")
        manifest = json.loads(archive.extractfile("bootstrap.json").read())
        if manifest.get("schema") != 1 or set(names) != set(manifest["files"]) | {"bootstrap.json"}:
            raise ValueError("Invalid bootstrap file inventory")
        if transport.get("through") != manifest.get("through") or not manifest.get("through"):
            raise ValueError("Bootstrap transport and archive dates differ")
        for name, sha in manifest["files"].items():
            if digest(archive.extractfile(name).read()) != sha:
                raise ValueError(f"Bootstrap hash mismatch: {name}")
        destination.mkdir(parents=True, exist_ok=True)
        for name in names:
            (destination / name).write_bytes(archive.extractfile(name).read())
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    build = commands.add_parser("package")
    build.add_argument("--evidence-dir", type=Path, required=True)
    build.add_argument("--provenance", type=Path, required=True)
    build.add_argument("--through", required=True)
    build.add_argument("--out", type=Path, required=True)
    build.add_argument("--assume-2027-holidays", action="store_true")
    download = commands.add_parser("fetch")
    download.add_argument("--url", required=True)
    download.add_argument("--out", type=Path, required=True)
    unpack = commands.add_parser("restore")
    unpack.add_argument("--archive", type=Path, required=True)
    unpack.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "package":
        package(args.evidence_dir, args.provenance, args.through, args.out, args.assume_2027_holidays)
    elif args.command == "fetch":
        fetch(args.url, args.out)
    else:
        restore(args.archive, args.out)


if __name__ == "__main__":
    main()
