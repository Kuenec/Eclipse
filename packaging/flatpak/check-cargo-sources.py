#!/usr/bin/env python3

import json
import sys
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SOURCES = Path("packaging/flatpak/cargo-sources.json")
LOCKFILES = (
    Path("Cargo.lock"),
    Path("crates/eclipse-webview/Cargo.lock"),
    Path("crates/libm-shim/Cargo.lock"),
)
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
VENDOR_PREFIX = "cargo/vendor/"


def locked_crates(problems):
    crates = {}
    for lockfile in LOCKFILES:
        with open(REPO / lockfile, "rb") as handle:
            packages = tomllib.load(handle).get("package", [])
        for package in packages:
            source = package.get("source")
            if source is None:
                continue
            crate = f"{package['name']}-{package['version']}"
            if source != CRATES_IO:
                problems.append(f"{lockfile}: {crate} comes from unsupported {source}")
                continue
            crates[crate] = package["checksum"]
    return crates


def vendored_crates(sources):
    archives = {}
    checksum_files = {}
    for source in sources:
        dest = source.get("dest", "")
        if not dest.startswith(VENDOR_PREFIX):
            continue
        crate = dest.removeprefix(VENDOR_PREFIX)
        if source["type"] == "archive":
            archives[crate] = source["sha256"]
        elif source.get("dest-filename") == ".cargo-checksum.json":
            checksum_files[crate] = json.loads(source["contents"])["package"]
    return archives, checksum_files


def main():
    problems = []
    crates = locked_crates(problems)
    with open(REPO / SOURCES, encoding="utf-8") as handle:
        sources = json.load(handle)
    archives, checksum_files = vendored_crates(sources)
    cargo_files = [
        source.get("dest-filename")
        for source in sources
        if source.get("dest") == "cargo"
    ]
    if cargo_files != ["config.toml"]:
        problems.append(f"cargo/ receives {cargo_files}, not only config.toml")
    for crate, checksum in sorted(crates.items()):
        if archives.get(crate) != checksum:
            problems.append(
                f"{crate}: no {VENDOR_PREFIX}{crate} archive with {checksum}"
            )
        if checksum_files.get(crate) != checksum:
            problems.append(f"{crate}: no .cargo-checksum.json for {checksum}")
    for crate in sorted(archives.keys() - crates.keys()):
        problems.append(f"{crate}: vendored but absent from every lockfile")
    if problems:
        print(
            f"{SOURCES} does not match the Cargo.lock files; run "
            "packaging/flatpak/update-cargo-sources.sh and commit the result:",
            file=sys.stderr,
        )
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    print(f"{SOURCES} vendors all {len(crates)} locked crates")
    return 0


if __name__ == "__main__":
    sys.exit(main())
