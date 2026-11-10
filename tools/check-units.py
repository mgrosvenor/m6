#!/usr/bin/env python3
"""Fail when a unit declares RuntimeDirectory without RuntimeDirectoryPreserve=yes.

RuntimeDirectory makes systemd create /run/m6 and delete it on stop. Several
units share that directory. When one of them stops or restarts, systemd deletes
the directory and every other service's socket under it, so m6-http exits 2 with
a socket glob that matches nothing. RuntimeDirectoryPreserve=yes keeps the
directory across stop and restart.

Scans m6's own docs and, when present, a sibling m6-examples checkout. A unit is
a real .service/.socket/.mount file or an ```ini fenced block in a markdown file.
"""

import argparse
import os
import re
import sys

RUNTIME_DIR = re.compile(r"^RuntimeDirectory\s*=\s*\S", re.MULTILINE)
PRESERVE = re.compile(r"^RuntimeDirectoryPreserve\s*=\s*yes\s*$", re.MULTILINE)

UNIT_EXTENSIONS = (".service", ".socket", ".mount")


def md_ini_blocks(path):
    """Yield the text of each ```ini fenced block in a markdown file."""
    try:
        with open(path, encoding="utf-8") as f:
            lines = f.read().splitlines()
    except OSError:
        return
    in_block = False
    buf = []
    for line in lines:
        stripped = line.strip()
        if not in_block:
            if stripped == "```ini":
                in_block = True
                buf = []
            continue
        if stripped.startswith("```"):
            if buf:
                yield "\n".join(buf) + "\n"
            in_block = False
            buf = []
        else:
            buf.append(line)


def unit_texts(root):
    """Yield (path, text) for every unit file and every fenced ini block."""
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in (".git", "target")]
        for name in filenames:
            path = os.path.join(dirpath, name)
            if name.endswith(UNIT_EXTENSIONS):
                try:
                    with open(path, encoding="utf-8") as f:
                        yield path, f.read()
                except OSError:
                    continue
            elif name.endswith(".md"):
                for block in md_ini_blocks(path):
                    yield path, block


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--examples", help="path to an m6-examples checkout")
    args = parser.parse_args()

    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)

    examples = args.examples
    if not examples:
        sibling = os.path.join(os.path.dirname(root), "m6-examples")
        if os.path.isfile(os.path.join(sibling, "Cargo.toml")):
            examples = sibling

    roots = [root]
    if examples:
        roots.append(examples)

    failures = []
    for base in roots:
        for path, text in unit_texts(base):
            if RUNTIME_DIR.search(text) and not PRESERVE.search(text):
                failures.append(path)

    if failures:
        for path in failures:
            print("RuntimeDirectory without RuntimeDirectoryPreserve=yes:", path)
        print(
            "%d unit(s) declare RuntimeDirectory without RuntimeDirectoryPreserve=yes"
            % len(failures)
        )
        return 1
    print("every unit that declares RuntimeDirectory sets RuntimeDirectoryPreserve=yes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
