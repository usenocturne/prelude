#!/usr/bin/env python3
"""Scan paths for CRLF in text-like files. Exit 1 if any found when --strict."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

EXTS = {
    ".sh",
    ".bash",
    ".service",
    ".timer",
    ".path",
    ".mount",
    ".target",
    ".socket",
    ".conf",
    ".rules",
    ".preset",
    ".cfg",
    ".inc",
    ".bb",
    ".bbappend",
    ".yml",
    ".yaml",
    ".toml",
    ".json",
    ".jsonc",
    ".py",
    ".txt",
    ".md",
    ".dtsi",
    ".dts",
    ".in",
    ".init",
    ".env",
}

SKIP_DIR_NAMES = {".git", "__pycache__", "node_modules", "build", "tmp"}


def is_candidate(path: Path) -> bool:
    if path.suffix.lower() in EXTS:
        return True
    # Anything installed from files/ can land on the rootfs (helpers, motd, issue).
    norm = path.as_posix()
    if "/files/" in norm or path.parent.name == "files":
        return True
    # Extensionless helpers under scripts/ or recipes often become /usr/bin shebangs.
    if path.suffix == "":
        try:
            return path.read_bytes()[:2] == b"#!"
        except OSError:
            return False
    if "/scripts/" in norm:
        try:
            return path.read_bytes()[:2] == b"#!"
        except OSError:
            return False
    return False


def scan(roots: list[Path]) -> list[tuple[int, Path]]:
    found: list[tuple[int, Path]] = []
    for root in roots:
        if not root.exists():
            continue
        for path in root.rglob("*"):
            if not path.is_file():
                continue
            if any(part in SKIP_DIR_NAMES for part in path.parts):
                continue
            if not is_candidate(path):
                continue
            try:
                data = path.read_bytes()
            except OSError:
                continue
            if b"\0" in data:
                continue
            count = data.count(b"\r")
            if count:
                found.append((count, path))
    return sorted(found, key=lambda item: (-item[0], str(item[1])))


def fix(paths: list[Path]) -> int:
    fixed = 0
    for path in paths:
        data = path.read_bytes()
        if b"\0" in data or b"\r" not in data:
            continue
        path.write_bytes(data.replace(b"\r\n", b"\n").replace(b"\r", b"\n"))
        fixed += 1
    return fixed


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("roots", nargs="+", type=Path)
    parser.add_argument("--fix", action="store_true")
    parser.add_argument("--strict", action="store_true")
    args = parser.parse_args()

    found = scan(args.roots)
    if args.fix and found:
        n = fix([path for _, path in found])
        print(f"fixed {n} files", flush=True)
        found = scan(args.roots)

    for count, path in found:
        print(f"{count:5d}  {path}")
    print(f"TOTAL_CRLF={len(found)}", flush=True)
    if args.strict and found:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
