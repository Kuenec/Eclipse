#!/usr/bin/env python3

import argparse
import json
import re
import sys
from pathlib import Path

SCHEMA = 1
KEPT_VERSIONS = 40
ECLIPSE_VERSION = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")


class ListError(Exception):
    pass


def eclipse_version(text):
    version = ECLIPSE_VERSION.match(text) if isinstance(text, str) else None
    if version is None:
        raise ListError(f"{text!r} is not an Eclipse version")
    return tuple(int(part) for part in version.groups())


def read_list(text):
    try:
        document = json.loads(text)
    except json.JSONDecodeError as error:
        raise ListError(f"the list is not JSON: {error}") from error
    if not isinstance(document, dict) or document.get("schema") != SCHEMA:
        raise ListError(f"the list does not have schema {SCHEMA}")
    entries = document.get("roblox")
    if not isinstance(entries, list):
        raise ListError("the list has no roblox versions")
    for entry in entries:
        code = entry.get("version_code") if isinstance(entry, dict) else None
        if type(code) is not int or not isinstance(entry.get("version_name"), str):
            raise ListError(f"{entry!r} names no Roblox version")
        eclipse_version(entry.get("eclipse"))
    return entries


def write_list(entries):
    return json.dumps({"schema": SCHEMA, "roblox": entries}, indent=2) + "\n"


def tested(entries, code, eclipse):
    return any(
        entry["version_code"] == code
        and eclipse_version(entry["eclipse"]) <= eclipse_version(eclipse)
        for entry in entries
    )


def add(entries, code, name, eclipse):
    eclipse_version(eclipse)
    earliest = min(
        [entry["eclipse"] for entry in entries if entry["version_code"] == code] + [eclipse],
        key=eclipse_version,
    )
    others = [entry for entry in entries if entry["version_code"] != code]
    added = {"version_code": code, "version_name": name, "eclipse": earliest}
    newest_first = sorted(others + [added], key=lambda entry: entry["version_code"], reverse=True)
    return newest_first[:KEPT_VERSIONS]


def parse_arguments(argv):
    parser = argparse.ArgumentParser(
        description="Read or extend the list of Roblox versions that start with Eclipse, "
        "which Eclipse reads before it installs a newer Roblox."
    )
    parser.add_argument("action", choices=["status", "add"])
    parser.add_argument("--list", type=Path, required=True, metavar="FILE")
    parser.add_argument("--roblox-code", type=int, required=True, metavar="VERSIONCODE")
    parser.add_argument("--roblox-name", metavar="VERSION", help="needed to add a version")
    parser.add_argument(
        "--eclipse", required=True, metavar="VERSION", help="the Eclipse that started this Roblox"
    )
    return parser.parse_args(argv)


def main(argv):
    arguments = parse_arguments(argv)
    try:
        eclipse_version(arguments.eclipse)
        entries = []
        if arguments.list.exists():
            entries = read_list(arguments.list.read_text("utf-8"))
        if arguments.action == "status":
            known = tested(entries, arguments.roblox_code, arguments.eclipse)
            print("tested" if known else "untested")
            return 0
        if not arguments.roblox_name:
            raise ListError("--roblox-name is needed to add a version")
        extended = add(entries, arguments.roblox_code, arguments.roblox_name, arguments.eclipse)
        arguments.list.write_text(write_list(extended), "utf-8")
    except (OSError, ListError) as error:
        print(f"record.py: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
