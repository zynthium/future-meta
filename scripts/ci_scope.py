"""Choose lightweight documentation checks or the full offline CI suite."""

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import subprocess


def documentation_only(paths):
    def is_documentation(path):
        value = PurePosixPath(path)
        return (
            path in {"README.md", "AGENTS.md", "LICENSE", "scripts/README.md"}
            or (value.parts[0] == "docs" and value.suffix == ".md")
            or (
                value.parts[0] == "skills"
                and (
                    value.suffix == ".md"
                    or value.parts[-2:] == ("agents", "openai.yaml")
                )
            )
        )

    return all(is_documentation(path) for path in paths)


def changed_paths(repo, base, head="HEAD"):
    if base and set(base) != {"0"}:
        args = ["diff", "--name-only", "--no-renames", "-z", base, head, "--"]
    else:
        args = ["ls-tree", "-r", "--name-only", "-z", head]
    result = subprocess.run(
        ["git", "-C", str(repo), *args], check=True, capture_output=True
    )
    return result.stdout.decode().rstrip("\0").split("\0") if result.stdout else []


def event_base(event_name, event):
    if event_name == "pull_request":
        return event["pull_request"]["base"]["sha"]
    if event_name == "push":
        return event["before"]
    raise ValueError(f"Unsupported CI event: {event_name}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", help="Compare a local commit range instead of a GitHub event")
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args()
    base = args.base
    if base is None:
        event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
        base = event_base(os.environ["GITHUB_EVENT_NAME"], event)
    paths = changed_paths(Path.cwd(), base)
    docs_only = documentation_only(paths)
    if args.github_output:
        with args.github_output.open("a") as output:
            output.write(f"docs_only={str(docs_only).lower()}\n")
    print(json.dumps({"docs_only": docs_only, "changed_files": len(paths)}))


if __name__ == "__main__":
    main()
