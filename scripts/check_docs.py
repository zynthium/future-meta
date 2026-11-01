"""Check repository Markdown links, code fences and skill metadata offline."""

from pathlib import Path
import re
import subprocess
from urllib.parse import unquote, urlsplit

import yaml


def prose_without_fences(text, path):
    fence = None
    prose = []
    for line in text.splitlines():
        match = re.match(r"^ {0,3}(`{3,}|~{3,})(.*)$", line)
        if match:
            marker, tail = match.groups()
            if fence is None:
                fence = (marker[0], len(marker))
            elif marker[0] == fence[0] and len(marker) >= fence[1] and not tail.strip():
                fence = None
        elif fence is None:
            prose.append(line)
    if fence is not None:
        raise ValueError(f"{path}: unclosed code fence")
    return "\n".join(prose)


def check_markdown(path):
    prose = prose_without_fences(path.read_text(), path)
    for match in re.finditer(r"\]\((<[^>]+>|[^\s)]+)(?:\s+[^)]*)?\)", prose):
        target = urlsplit(match.group(1).strip("<>"))
        if target.scheme or target.netloc or not target.path:
            continue
        if not (path.parent / unquote(target.path)).exists():
            raise ValueError(f"{path}: missing local link target {target.path}")


def check_skill(path):
    text = path.read_text()
    frontmatter = re.match(r"\A---\n(.*?)\n---(?:\n|$)", text, re.S)
    if frontmatter is None:
        raise ValueError(f"{path}: missing skill frontmatter")
    metadata = yaml.safe_load(frontmatter.group(1))
    if not isinstance(metadata, dict) or metadata.get("name") != path.parent.name:
        raise ValueError(f"{path}: skill name must match its directory")
    if not isinstance(metadata.get("description"), str) or not metadata["description"].strip():
        raise ValueError(f"{path}: skill description is required")
    interface_path = path.parent / "agents/openai.yaml"
    if interface_path.exists():
        interface = yaml.safe_load(interface_path.read_text())["interface"]
        description = interface["short_description"]
        if not isinstance(description, str) or not 25 <= len(description) <= 64:
            raise ValueError(f"{interface_path}: short_description must have 25-64 characters")
        if "$" + metadata["name"] not in interface["default_prompt"]:
            raise ValueError(f"{interface_path}: default_prompt must invoke its skill")


def main():
    root = Path(__file__).resolve().parents[1]
    files = subprocess.run(
        ["git", "-C", str(root), "ls-files", "--cached", "--others", "--exclude-standard", "-z", "--", "*.md"],
        check=True,
        capture_output=True,
    ).stdout.decode().rstrip("\0").split("\0")
    for name in files:
        path = root / name
        if path.is_file():
            check_markdown(path)
    for path in (root / "skills").glob("*/SKILL.md"):
        check_skill(path)
    print("Markdown file links, code fences and skill metadata are valid")


if __name__ == "__main__":
    main()
