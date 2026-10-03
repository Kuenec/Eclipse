#!/usr/bin/env python3

import argparse
import re
import sys
import tomllib
from pathlib import Path
from xml.etree import ElementTree

REPO = Path(__file__).resolve().parents[2]
CARGO_TOML = Path("Cargo.toml")
METAINFO = Path("packaging/flatpak/io.github.kuenec.Eclipse.metainfo.xml")
INLINE_SYNTAX = re.compile(r"([\\`*_\[\]<>&~])")
BLOCK_START = re.compile(r"^(\d*)([#+=.)-])")
PINNED_IMAGE = re.compile(
    r"https://raw\.githubusercontent\.com/[^/]+/[^/]+/(v\d+\.\d+\.\d+|[0-9a-f]{40})/"
)


class NotesError(Exception):
    pass


def cargo_version():
    try:
        with open(REPO / CARGO_TOML, "rb") as handle:
            return tomllib.load(handle)["package"]["version"]
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise NotesError(f"cannot read {CARGO_TOML}: {error}") from None
    except KeyError:
        raise NotesError(f"{CARGO_TOML} has no package.version") from None


def metainfo_root():
    try:
        return ElementTree.parse(REPO / METAINFO).getroot()
    except (OSError, ElementTree.ParseError) as error:
        raise NotesError(f"cannot read {METAINFO}: {error}") from None


def release_description(root, version):
    releases = [
        release
        for release in root.iterfind("releases/release")
        if release.get("version") == version
    ]
    if len(releases) != 1:
        raise NotesError(
            f"must appear once, with its notes, and it appears {len(releases)} times"
        )
    description = releases[0].find("description")
    if description is None or not "".join(description.itertext()).strip():
        raise NotesError("has no <description> text; write the release notes there")
    return description


def require_pinned_screenshots(root, version):
    for image in root.iterfind("screenshots/screenshot/image"):
        url = (image.text or "").strip()
        if not PINNED_IMAGE.match(url):
            raise NotesError(
                f"{METAINFO}: screenshot {url} is not pinned to a tag or commit on "
                "raw.githubusercontent.com; use the first tag that contains the "
                f"image, such as v{version}"
            )


def unsupported(element):
    return NotesError(
        f"uses <{element.tag}>; release notes support only <p>, <ul>, <li> and <code>"
    )


def require_blank(text):
    if text and text.strip():
        raise NotesError(f"has text outside <p> and <li>: {text.strip()!r}")


def escaped(text):
    return INLINE_SYNTAX.sub(r"\\\1", text or "")


def code_span(element):
    if len(element):
        raise unsupported(element[0])
    code = " ".join((element.text or "").split())
    if not code or "`" in code:
        raise NotesError(f"has a <code> that is empty or holds a backtick: {code!r}")
    return f"`{code}`"


def block_text(element):
    parts = [escaped(element.text)]
    for child in element:
        if child.tag != "code":
            raise unsupported(child)
        parts.append(code_span(child))
        parts.append(escaped(child.tail))
    text = " ".join("".join(parts).split())
    return BLOCK_START.sub(r"\1\\\2", text)


def markdown(description):
    require_blank(description.text)
    blocks = []
    for element in description:
        require_blank(element.tail)
        if element.tag == "p":
            blocks.append(block_text(element))
        elif element.tag == "ul":
            require_blank(element.text)
            items = []
            for item in element:
                require_blank(item.tail)
                if item.tag != "li":
                    raise unsupported(item)
                items.append(f"- {block_text(item)}")
            blocks.append("\n".join(items))
        else:
            raise unsupported(element)
    return "\n\n".join(blocks) + "\n"


def release_notes(requested):
    version = cargo_version()
    if requested is not None and requested != version:
        raise NotesError(
            f"asked for the notes of {requested}, but {CARGO_TOML} has version "
            f"{version}; tag the commit that bumps the version"
        )
    root = metainfo_root()
    if requested is not None:
        require_pinned_screenshots(root, version)
    try:
        return markdown(release_description(root, version))
    except NotesError as error:
        raise NotesError(f'{METAINFO}: <release version="{version}"> {error}') from None


def main():
    parser = argparse.ArgumentParser(
        description="Print the metainfo release notes for the version in "
        "Cargo.toml as Markdown."
    )
    parser.add_argument(
        "version",
        nargs="?",
        help="fail unless Cargo.toml has this version and every screenshot is "
        "pinned to a tag or commit; give a tag without its v",
    )
    arguments = parser.parse_args()
    try:
        notes = release_notes(arguments.version)
    except NotesError as error:
        print(f"release-notes.py: {error}", file=sys.stderr)
        return 1
    sys.stdout.write(notes)
    return 0


if __name__ == "__main__":
    sys.exit(main())
