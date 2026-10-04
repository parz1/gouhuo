#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Keep the frontend library graph free of codec/APM and GUI implementations."""

import pathlib
import subprocess
import sys


def main() -> int:
    root = pathlib.Path(__file__).resolve().parent.parent
    for package in ("client-runtime", "client-process", "client"):
        result = subprocess.run(
            ["cargo", "tree", "--locked", "-p", package,
             "--no-default-features", "--edges", "normal", "--prefix", "none"],
            cwd=root, capture_output=True, text=True, encoding="utf-8", check=True,
        )
        packages = {line.split()[0] for line in result.stdout.splitlines() if line.strip()}
        forbidden = {"opus", "audiopus_sys", "sonora", "sonora-sys", "voice-engine"}
        if package != "client":
            forbidden.add("slint")
        found = sorted(packages & forbidden)
        if found:
            print(f"{package} pulled in engine dependencies: {', '.join(found)}", file=sys.stderr)
            return 1
        print(f"{package}: frontend dependency boundary passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
