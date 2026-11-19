#!/usr/bin/env python3

import argparse
import base64
import io
import re
import sys
import tarfile
import urllib.request
from pathlib import Path

GITILES = "https://android.googlesource.com/platform"
RELEASE_TAG = "android-15.0.0_r1"
RUSTFMT_TUPLE_WIDTH = 60
MAPS = (
    ("frameworks/base", "native/android/libandroid.map.txt"),
    ("frameworks/av", "media/ndk/libmediandk.map.txt"),
    ("system/logging", "liblog/liblog.map.txt"),
)
HEADER_DIRECTORIES = (
    ("frameworks/native", "include/android"),
    ("frameworks/native", "libs/nativewindow/include/android"),
    ("frameworks/native", "libs/arect/include/android"),
    ("frameworks/av", "media/ndk/include/media"),
    ("system/logging", "liblog/include/android"),
)
COMMENT_OR_STRING = re.compile(r'"(?:\\.|[^"\\\n])*"|/\*.*?\*/|//[^\n]*', re.S)
DIRECTIVE = re.compile(r"^[ \t]*#(?:[^\n]*\\\n)*[^\n]*$", re.M)
ANNOTATION = re.compile(
    r"\b__(?:INTRODUCED_IN|DEPRECATED_IN|REMOVED_IN|INTRODUCED_IN_LLNDK|ANDROID_UNAVAILABLE)"
    r"\s*\([^()]*(?:\([^()]*\)[^()]*)*\)"
    r"|__attribute__\s*\(\([^;]*?\)\)|\b__NDK_FPABI__\b|\b__nonnull\b|\b_Nonnull\b|\b_Nullable\b"
)
PROTOTYPE = re.compile(r"^(?P<returns>[\w \t\n\*]+?)\b(?P<name>\w+)\s*\((?P<parameters>.*)\)$", re.S)
MAP_ENTRY = re.compile(r"^\s*(?P<name>\w+);(?:\s*#\s*(?P<tags>.*))?$")
UNSIGNED = re.compile(r"\b(?:size_t|unsigned|uint\d+_t|uintptr_t)\b")
STORAGE = re.compile(r"\b(?:extern|static|inline|const|struct|enum)\b")


class SurfaceError(Exception):
    pass


def exported_functions(map_text):
    names = []
    platform_only = False
    for line in map_text.splitlines():
        if "{" in line:
            platform_only = "PLATFORM" in line or "PRIVATE" in line
            continue
        entry = MAP_ENTRY.match(line)
        if entry is None or platform_only:
            continue
        tags = (entry["tags"] or "").split()
        if "var" in tags:
            continue
        names.append(entry["name"])
    return names


def prototypes(header_text):
    text = DIRECTIVE.sub("", COMMENT_OR_STRING.sub(" ", header_text))
    found = {}
    for statement in re.split(r"[;{}]", ANNOTATION.sub(" ", text)):
        match = PROTOTYPE.match(statement.strip())
        if match is None or "typedef" in match["returns"]:
            continue
        returns = " ".join(STORAGE.sub(" ", match["returns"]).split())
        if returns:
            found[match["name"]] = returns
    return found


def answer(returns):
    if "*" in returns:
        return "Zero"
    if returns == "void":
        return "Zero"
    if returns == "bool":
        return "Zero"
    if returns in ("float", "double"):
        return "FloatZero"
    if returns == "media_status_t":
        return "MediaUnsupported"
    if UNSIGNED.search(returns):
        return "Zero"
    return "MinusOne"


def surface(maps, headers):
    declared = {}
    for header in headers:
        declared.update(prototypes(header))
    answers = {}
    undeclared = []
    for map_text in maps:
        for name in exported_functions(map_text):
            if name in declared:
                answers[name] = answer(declared[name])
            else:
                undeclared.append(name)
    return answers, sorted(set(undeclared) - set(answers))


def rust_source(answers):
    lines = [
        "use super::Answer;",
        "",
        "pub(super) const FUNCTIONS: &[(&str, Answer)] = &[",
    ]
    for name in sorted(answers):
        fields = f'"{name}", Answer::{answers[name]}'
        if len(fields) > RUSTFMT_TUPLE_WIDTH:
            fields = f'\n        "{name}",\n        Answer::{answers[name]},\n    '
        lines.append(f"    ({fields}),")
    lines += ["];", ""]
    return "\n".join(lines)


def fetch(url):
    with urllib.request.urlopen(url, timeout=120) as response:
        return response.read()


def fetch_sources():
    ref = f"refs/tags/{RELEASE_TAG}"
    maps = [
        base64.b64decode(fetch(f"{GITILES}/{project}/+/{ref}/{path}?format=TEXT")).decode()
        for project, path in MAPS
    ]
    headers = []
    for project, directory in HEADER_DIRECTORIES:
        archive = fetch(f"{GITILES}/{project}/+archive/{ref}/{directory}.tar.gz")
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
            for member in tar:
                if member.isfile() and member.name.endswith(".h"):
                    headers.append(tar.extractfile(member).read().decode("utf-8", "replace"))
    return maps, headers


def read_sources(directory):
    maps = [path.read_text("utf-8") for path in sorted(directory.glob("*.map.txt"))]
    headers = [path.read_text("utf-8", "replace") for path in sorted(directory.rglob("*.h"))]
    if not maps or not headers:
        raise SurfaceError(f"{directory} holds no *.map.txt or no headers")
    return maps, headers


def parse_arguments(argv):
    parser = argparse.ArgumentParser(
        description="Write the table of public NDK functions and the answer each gives "
        "when Eclipse does not implement it."
    )
    parser.add_argument(
        "--sources",
        type=Path,
        metavar="DIR",
        help=f"read the map files and headers from DIR instead of downloading {RELEASE_TAG}",
    )
    parser.add_argument("--out", type=Path, required=True, metavar="FILE")
    return parser.parse_args(argv)


def main(argv):
    arguments = parse_arguments(argv)
    try:
        maps, headers = (
            read_sources(arguments.sources) if arguments.sources else fetch_sources()
        )
        answers, undeclared = surface(maps, headers)
        arguments.out.write_text(rust_source(answers), "utf-8")
    except (OSError, SurfaceError, tarfile.TarError, ValueError) as error:
        print(f"generate.py: {error}", file=sys.stderr)
        return 1
    print(f"{len(answers)} functions; {len(undeclared)} exported without a public declaration")
    for name in undeclared:
        print(f"  {name}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
