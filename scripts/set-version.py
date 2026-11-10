#!/usr/bin/env python3
"""Set the workspace version using the calendar scheme.

Versions use ``YYYY.MDD.REVISION`` in Asia/Hong_Kong: the month is not padded,
the day is two digits, and the first release of a day is revision zero. For
example, ``2026.1110.0`` is the first release on 2026-11-10.

The script updates the workspace version in ``Cargo.toml`` and the matching
workspace package entries in ``Cargo.lock``, then verifies the lock with
``cargo metadata --locked``. Cargo.lock is tracked from the first release
onward so release builds can use ``--locked``.
"""
from __future__ import annotations

import re
import subprocess
import sys
from datetime import date
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VERSION_PATTERN = re.compile(r"^(\d{4})\.(\d{3,4})\.(\d+)$")
WORKSPACE_PACKAGES = ("rc-protocol", "rc-agent", "rc-bridge", "rc-mapping", "rc-helper", "rc-app")


def validate_version(version: str) -> None:
    match = VERSION_PATTERN.fullmatch(version)
    if not match:
        raise SystemExit("version must use YYYY.MDD.REVISION, for example 2026.1110.0")
    year, month_day, _ = match.groups()
    month = int(month_day[:-2])
    day = int(month_day[-2:])
    try:
        parsed = date(int(year), month, day)
    except ValueError as error:
        raise SystemExit(f"invalid calendar version {version}: {error}") from error
    if month_day != f"{parsed.month}{parsed.day:02d}":
        raise SystemExit("month must be unpadded and day must be two digits")


def replace_once(path: Path, pattern: str, replacement: str) -> None:
    content = path.read_text()
    updated, count = re.subn(pattern, replacement, content, flags=re.MULTILINE)
    if count != 1:
        raise SystemExit(f"expected one version field in {path.name}, found {count}")
    path.write_text(updated)


def update_lockfile(version: str) -> None:
    lock = ROOT / "Cargo.lock"
    if not lock.exists():
        return
    content = lock.read_text()
    for package in WORKSPACE_PACKAGES:
        content, count = re.subn(
            rf'(name = "{re.escape(package)}"\nversion = ")[^"]+(")',
            rf"\g<1>{version}\g<2>",
            content,
        )
        if count != 1:
            raise SystemExit(f"expected one lock entry for {package}, found {count}")
    lock.write_text(content)


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {Path(sys.argv[0]).name} <YYYY.MDD.REVISION>")
    version = sys.argv[1]
    validate_version(version)

    replace_once(ROOT / "Cargo.toml", r'^version = "\d+\.\d+\.\d+"$', f'version = "{version}"')
    update_lockfile(version)

    # --locked proves the committed lockfile agrees with the manifests.
    subprocess.run(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        stdout=subprocess.DEVNULL,
        check=True,
    )
    print(f"Set Remote Mic Rune version to {version}")


if __name__ == "__main__":
    main()
