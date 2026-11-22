#!/usr/bin/env python3

import argparse
import re
import shutil
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path

ENTRY = re.compile(
    r"^(?P<kind>method|field) (?P<binding>static|instance) "
    r"(?P<owner>L[^;>]+;)->(?P<member>\S+)$"
)
METHOD = re.compile(r"^(?P<name>[^(]+)\((?P<parameters>[^)]*)\)(?P<returns>\S+)$")
PARAMETER = re.compile(r"\[*(?:L[^;]+;|[ZBSCIJFD])")
SUPERCLASS = re.compile(r"^\.super (L[^;]+;)$", re.M)
OBJECT = "Ljava/lang/Object;"
CONSTRUCTOR = "<init>"
NO_ARGUMENT_CONSTRUCTOR = "<init>()V"
REPORT = "Landroid/util/EclipseMissingApi;->used(Ljava/lang/String;)V"
LOCAL_REGISTERS = 2


class SurfaceError(Exception):
    pass


@dataclass(frozen=True)
class Member:
    kind: str
    static: bool
    owner: str
    member: str

    @property
    def id(self):
        return f"{self.owner}->{self.member}"


def read_members(text):
    members = []
    for number, line in enumerate(text.splitlines(), 1):
        entry = ENTRY.match(line)
        if entry is None:
            raise SurfaceError(f"line {number} is not a missing member: {line!r}")
        if entry["kind"] == "method" and METHOD.match(entry["member"]) is None:
            raise SurfaceError(f"line {number} names no method: {line!r}")
        if entry["kind"] == "field" and ":" not in entry["member"]:
            raise SurfaceError(f"line {number} names no field: {line!r}")
        members.append(
            Member(entry["kind"], entry["binding"] == "static", entry["owner"], entry["member"])
        )
    return members


def declared(smali):
    members = set()
    for line in smali.splitlines():
        words = line.split()
        if not words:
            continue
        if words[0] == ".method":
            members.add(words[-1])
        elif words[0] == ".field":
            members.add(next(word for word in words[1:] if ":" in word))
    return members


def parameter_words(parameters):
    return sum(2 if parameter in ("J", "D") else 1 for parameter in PARAMETER.findall(parameters))


def default_return(member, returns):
    if not member.static and returns == member.owner:
        return ["    return-object p0"]
    if returns == "V":
        return ["    return-void"]
    if returns in ("J", "D"):
        return ["    const-wide/16 v0, 0x0", "", "    return-wide v0"]
    if returns[0] in "L[":
        return ["    const/4 v0, 0x0", "", "    return-object v0"]
    return ["    const/4 v0, 0x0", "", "    return v0"]


def method_stub(member, chained_constructor):
    method = METHOD.match(member.member)
    registers = LOCAL_REGISTERS + parameter_words(method["parameters"]) + (not member.static)
    flags = "public static" if member.static else "public"
    lines = []
    if method["name"] == CONSTRUCTOR:
        flags = "public constructor"
        lines = [f"    invoke-direct {{p0}}, {chained_constructor}->{NO_ARGUMENT_CONSTRUCTOR}", ""]
    return "\n".join(
        [
            "",
            f".method {flags} {member.member}",
            f"    .registers {registers}",
            "",
            *lines,
            f'    const-string v0, "{member.id}"',
            "",
            f"    invoke-static {{v0}}, {REPORT}",
            "",
            *default_return(member, method["returns"]),
            ".end method",
            "",
        ]
    )


def field_stub(member, interface):
    flags = "public static" if member.static else "public"
    if interface:
        flags = "public static final"
    return f"\n.field {flags} {member.member}\n"


class Framework:
    def __init__(self, view, stock, fixed):
        self.view = view
        self.stock = stock
        self.fixed = fixed

    @staticmethod
    def _file(root, owner):
        return root / f"{owner[1:-1]}.smali"

    def text(self, owner):
        for root in (self.fixed, self.view, self.stock):
            file = self._file(root, owner)
            if file.is_file():
                return file.read_text("utf-8")
        return None

    def editable(self, owner):
        in_fixed = self._file(self.fixed, owner)
        if in_fixed.is_file():
            return in_fixed
        in_view = self._file(self.view, owner)
        if in_view.is_file():
            return in_view
        in_stock = self._file(self.stock, owner)
        if not in_stock.is_file():
            return None
        in_view.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(in_stock, in_view)
        return in_view

    def chained_constructor(self, owner, smali):
        if NO_ARGUMENT_CONSTRUCTOR in declared(smali):
            return owner
        parent = SUPERCLASS.search(smali)
        if parent is None:
            return None
        if parent[1] == OBJECT:
            return OBJECT
        inherited = self.text(parent[1])
        if inherited is not None and NO_ARGUMENT_CONSTRUCTOR in declared(inherited):
            return parent[1]
        return None


def stub_for(member, smali, framework):
    header = smali.split("\n", 1)[0].split()
    interface = "interface" in header
    if "annotation" in header:
        return None, "its class is an annotation"
    if member.kind == "field":
        if interface and not member.static:
            return None, "an interface has no instance fields"
        return field_stub(member, interface), None
    chained = None
    if member.member.startswith(CONSTRUCTOR):
        chained = framework.chained_constructor(member.owner, smali)
        if interface or chained is None:
            return None, "no constructor without arguments to build on"
    return method_stub(member, chained), None


def add_missing(members, framework):
    outcomes = Counter()
    skipped = []
    for member in members:
        smali = framework.text(member.owner)
        if smali is None:
            skipped.append((member, "the framework does not define its class"))
            continue
        if member.member in declared(smali):
            outcomes["defined"] += 1
            continue
        stub, reason = stub_for(member, smali, framework)
        if stub is None:
            skipped.append((member, reason))
            continue
        with framework.editable(member.owner).open("a", encoding="utf-8") as out:
            out.write(stub)
        outcomes["added"] += 1
    return outcomes, skipped


def parse_arguments(argv):
    parser = argparse.ArgumentParser(
        description="Give framework classes the methods and fields a Roblox client "
        "references and they lack. Each added method reports its first call and "
        "returns zero, false or null, or the object itself when it returns its own "
        "class, as a builder's setters do."
    )
    parser.add_argument("--surface", type=Path, required=True, metavar="FILE")
    parser.add_argument(
        "--view",
        type=Path,
        required=True,
        metavar="DIR",
        help="the smali tree assembled into the overlay dex; changed classes land here",
    )
    parser.add_argument(
        "--stock", type=Path, required=True, metavar="DIR", help="the stock framework's smali"
    )
    parser.add_argument(
        "--fixed",
        type=Path,
        required=True,
        metavar="DIR",
        help="the smali of the first dex, built from Eclipse's Java sources; its classes "
        "win over the overlay dex, so they change in place",
    )
    return parser.parse_args(argv)


def main(argv):
    arguments = parse_arguments(argv)
    try:
        members = read_members(arguments.surface.read_text("utf-8"))
        outcomes, skipped = add_missing(
            members, Framework(arguments.view, arguments.stock, arguments.fixed)
        )
    except (OSError, SurfaceError) as error:
        print(f"surface.py: {error}", file=sys.stderr)
        return 1
    print(
        f"{outcomes['added']} members added, {outcomes['defined']} already defined, "
        f"{len(skipped)} left out"
    )
    for member, reason in skipped:
        print(f"  {member.id}: {reason}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
