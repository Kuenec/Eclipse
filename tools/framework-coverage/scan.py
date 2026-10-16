#!/usr/bin/env python3

import argparse
import json
import re
import sys
import zipfile
from collections import defaultdict, deque
from dataclasses import dataclass
from enum import Enum
from pathlib import Path

from dex import (
    ACC_PRIVATE,
    ACC_STATIC,
    REF_TABLE,
    ClassDef,
    DexError,
    Ref,
    Table,
    element_type,
    read_dexes,
)
from manifest import ManifestError, read_components
from resources import (
    ResourceError,
    is_compiled_xml,
    read_compiled_xml,
    read_xml_files,
)

ANDROID_PREFIX = "Landroid/"
OBJECT = "Ljava/lang/Object;"
CONSTRUCTOR = ("<init>", "()V")
STATIC_INITIALIZER = ("<clinit>", "()V")
ATTACH_BASE_CONTEXT = ("attachBaseContext", "(Landroid/content/Context;)V")
APPLICATION_ENTRY = (ATTACH_BASE_CONTEXT, ("onCreate", "()V"))
PROVIDER_ENTRY = (
    ("attachInfo", "(Landroid/content/Context;Landroid/content/pm/ProviderInfo;)V"),
    ("onCreate", "()Z"),
)
ACTIVITY_ENTRY = (
    ATTACH_BASE_CONTEXT,
    ("onCreate", "(Landroid/os/Bundle;)V"),
    ("onPostCreate", "(Landroid/os/Bundle;)V"),
    ("onStart", "()V"),
    ("onResume", "()V"),
)
MANIFEST_ENTRY = "AndroidManifest.xml"
RESOURCE_TABLE_ENTRY = "resources.arsc"
NATIVE_LIBRARY = re.compile(r"lib/[^/]+/[^/]+\.so")
NATIVE_CLASS_NAME = re.compile(rb"\0([A-Za-z0-9_$]+(?:/[A-Za-z0-9_$]+)+)(?=\0)")


class GapKind(Enum):
    CLASS = "class"
    METHOD = "method"
    FIELD = "field"


@dataclass(frozen=True)
class Gap:
    kind: GapKind
    id: str


@dataclass(frozen=True)
class Defined:
    owner: ClassDef


@dataclass(frozen=True)
class UndefinedClass:
    name: str


@dataclass(frozen=True)
class UndefinedMember:
    pass


class Hierarchy:
    def __init__(self, framework, client):
        self.framework = framework
        self.client = {
            name: definition
            for name, definition in client.items()
            if name not in framework
        }
        self.classes = self.client | framework
        self._supertypes = {}

    def supertypes(self, name):
        known = self._supertypes.get(name)
        if known is not None:
            return known
        found = {name}
        definition = self.classes.get(name)
        if definition is not None:
            for parent in (definition.superclass, *definition.interfaces):
                if parent is not None:
                    found |= self.supertypes(parent)
        known = frozenset(found)
        self._supertypes[name] = known
        return known

    def superinterfaces(self, names):
        queue = deque(names)
        seen = set()
        while queue:
            name = queue.popleft()
            definition = self.classes.get(name)
            if name in seen or definition is None:
                continue
            seen.add(name)
            yield definition
            queue.extend(definition.interfaces)

    def resolve_method(self, name, key):
        current = name
        interfaces = []
        while current is not None:
            definition = self.classes.get(current)
            if definition is None:
                return UndefinedClass(current)
            if key in definition.methods:
                return Defined(definition)
            interfaces.extend(definition.interfaces)
            current = definition.superclass
        for definition in self.superinterfaces(interfaces):
            if key in definition.methods:
                return Defined(definition)
        return UndefinedMember()

    def resolve_field(self, name, key):
        current = name
        while current is not None:
            definition = self.classes.get(current)
            if definition is None:
                return UndefinedClass(current)
            if key in definition.fields:
                return Defined(definition)
            for interface in self.superinterfaces(definition.interfaces):
                if key in interface.fields:
                    return Defined(interface)
            current = definition.superclass
        return UndefinedMember()

    def android_owner(self, name):
        queue = deque([name])
        seen = set()
        while queue:
            current = queue.popleft()
            definition = self.classes.get(current)
            if current in seen or definition is None:
                continue
            seen.add(current)
            if current.startswith(ANDROID_PREFIX) and current in self.framework:
                return current
            if definition.superclass is not None:
                queue.append(definition.superclass)
            queue.extend(definition.interfaces)
        return None

    def inherited_from_object(self, key):
        return key[0] != CONSTRUCTOR[0] and key in self.classes[OBJECT].methods

    def missing_class(self, descriptor):
        name = element_type(descriptor)
        if name.startswith(ANDROID_PREFIX) and name not in self.classes:
            return Gap(GapKind.CLASS, name)
        return None


class GapFinder:
    def __init__(self, hierarchy):
        self.hierarchy = hierarchy
        self._gaps = {}

    def find(self):
        references = defaultdict(lambda: defaultdict(set))
        for definition in self.hierarchy.client.values():
            for parent in (definition.superclass, *definition.interfaces):
                gap = None if parent is None else self.hierarchy.missing_class(parent)
                if gap is not None:
                    references[gap][definition.name].add(None)
            for key, method in definition.methods.items():
                if not method.code_off:
                    continue
                for ref, index in definition.dex.code_refs(method.code_off):
                    for gap in self._gaps_of(definition.dex, REF_TABLE[ref], index):
                        references[gap][definition.name].add(key)
        return references

    def _gaps_of(self, dex, table, index):
        target = (dex, table, index)
        if target not in self._gaps:
            self._gaps[target] = self._resolve(dex, table, index)
        return self._gaps[target]

    def _resolve(self, dex, table, index):
        match table:
            case Table.TYPE:
                gap = self.hierarchy.missing_class(dex.type_name(index))
                return () if gap is None else (gap,)
            case Table.FIELD:
                owner, name, type_ = dex.field_ref(index)
                key = (name, type_)
                result = self.hierarchy.resolve_field(owner, key)
                return self._member_gaps(owner, result, GapKind.FIELD, key)
            case Table.METHOD:
                owner, name, proto = dex.method_ref(index)
                key = (name, proto)
                result = self.hierarchy.resolve_method(owner, key)
                return self._member_gaps(owner, result, GapKind.METHOD, key)

    def _member_gaps(self, owner, result, kind, key):
        match result:
            case Defined():
                return ()
            case UndefinedClass(name=missing):
                gap = self.hierarchy.missing_class(missing)
                if gap is None:
                    return ()
                if self.hierarchy.inherited_from_object(key):
                    return (gap,)
                return (gap, member_gap(gap.id, kind, key))
            case UndefinedMember():
                blamed = self.hierarchy.android_owner(owner)
                if blamed is None:
                    return ()
                return (member_gap(blamed, kind, key),)


def member_gap(owner, kind, key):
    name, descriptor = key
    separator = ":" if kind is GapKind.FIELD else ""
    return Gap(kind, f"{owner}->{name}{separator}{descriptor}")


class StartupReach:
    def __init__(self, hierarchy, entries, native_classes, xml_resources):
        self.hierarchy = hierarchy
        self.entries = frozenset(name for name, _ in entries)
        self.xml_resources = xml_resources
        self.methods = set()
        self.loaded = set()
        self._queue = []
        self._initialized = set()
        self._instantiated = set()
        self._inflated = set()
        self._dispatch_tables = {}
        self._implementers = defaultdict(list)
        self._call_sites = set()
        for name, keys in entries:
            self._enter(name, keys)
        for name in native_classes:
            self._expose(name)
        while self._queue:
            self._visit(*self._queue.pop())

    def loaded_classes(self):
        loaded = set()
        for name in self.loaded:
            loaded |= self.hierarchy.supertypes(name)
        return loaded

    def _enter(self, name, callbacks):
        if name not in self.hierarchy.client:
            return
        self._construct(name)
        dispatch = self._dispatch_table(name)
        for key in callbacks:
            if key in dispatch:
                self._reach(*dispatch[key])

    def _expose(self, name):
        definition = self.hierarchy.client.get(name)
        if definition is None:
            return
        self._instantiate(name)
        for key, method in definition.methods.items():
            self._reach(name, key, method)

    def _inflate(self, resource):
        if resource in self._inflated:
            return
        self._inflated.add(resource)
        for document in self.xml_resources.get(resource, ()):
            for text in document.strings:
                self._inflate_view(f"L{text.replace('.', '/')};")
            for reference in document.references:
                self._inflate(reference)

    def _inflate_view(self, name):
        definition = self.hierarchy.client.get(name)
        if definition is None:
            return
        self._instantiate(name)
        for key, method in definition.methods.items():
            if key[0] == CONSTRUCTOR[0]:
                self._reach(name, key, method)

    def _reach(self, owner, key, method):
        if not method.code_off or (owner, key) in self.methods:
            return
        self.methods.add((owner, key))
        self.loaded.add(owner)
        self._queue.append((owner, method))

    def _visit(self, owner, method):
        dex = self.hierarchy.classes[owner].dex
        for ref, index in dex.code_refs(method.code_off):
            match REF_TABLE[ref]:
                case Table.TYPE:
                    self._visit_type(ref, element_type(dex.type_name(index)))
                case Table.FIELD:
                    self._visit_field(ref, *dex.field_ref(index))
                case Table.METHOD:
                    self._visit_call(ref, *dex.method_ref(index))

    def _visit_type(self, ref, name):
        self.loaded.add(name)
        if ref is Ref.NEW_INSTANCE:
            self._instantiate(name)
        elif ref is Ref.CLASS_LITERAL:
            self._construct(name)

    def _visit_field(self, ref, owner, name, type_):
        self.loaded.add(owner)
        if ref is not Ref.STATIC_FIELD:
            return
        key = (name, type_)
        result = self.hierarchy.resolve_field(owner, key)
        if not isinstance(result, Defined):
            return
        self._initialize(result.owner.name)
        resource = result.owner.int_constants.get(key)
        if resource is not None:
            self._inflate(resource)

    def _visit_call(self, ref, owner, name, proto):
        key = (name, proto)
        self.loaded.add(element_type(owner))
        if ref is Ref.VIRTUAL_CALL:
            self._add_call_site(owner, key)
            return
        result = self.hierarchy.resolve_method(owner, key)
        if not isinstance(result, Defined):
            return
        target = result.owner.name
        if ref is Ref.STATIC_CALL:
            self._initialize(target)
        if target in self.hierarchy.client:
            self._reach(target, key, result.owner.methods[key])

    def _initialize(self, name):
        while name in self.hierarchy.client and name not in self._initialized:
            self._initialized.add(name)
            self.loaded.add(name)
            definition = self.hierarchy.client[name]
            initializer = definition.methods.get(STATIC_INITIALIZER)
            if initializer is not None:
                self._reach(name, STATIC_INITIALIZER, initializer)
            name = definition.superclass

    def _construct(self, name):
        self._instantiate(name)
        definition = self.hierarchy.client.get(name)
        if definition is not None and CONSTRUCTOR in definition.methods:
            self._reach(name, CONSTRUCTOR, definition.methods[CONSTRUCTOR])

    def _instantiate(self, name):
        if name not in self.hierarchy.client or name in self._instantiated:
            return
        self._instantiated.add(name)
        self._initialize(name)
        supertypes = self.hierarchy.supertypes(name)
        framework = self.hierarchy.framework
        overridable = []
        if name not in self.entries:
            overridable = [
                framework[type_] for type_ in supertypes if type_ in framework
            ]
        for key, target in self._dispatch_table(name).items():
            self._implementers[key].append(name)
            if any(key in parent.methods for parent in overridable) or any(
                (type_, key) in self._call_sites for type_ in supertypes
            ):
                self._reach(*target)

    def _add_call_site(self, receiver, key):
        site = (receiver, key)
        if site in self._call_sites:
            return
        self._call_sites.add(site)
        for name in self._implementers.get(key, ()):
            if receiver in self.hierarchy.supertypes(name):
                self._reach(*self._dispatch_tables[name][key])

    def _dispatch_table(self, name):
        table = self._dispatch_tables.get(name)
        if table is not None:
            return table
        table = {}
        interfaces = []
        current = name
        while current in self.hierarchy.client:
            definition = self.hierarchy.client[current]
            add_implementations(table, definition)
            interfaces.extend(definition.interfaces)
            current = definition.superclass
        for definition in self.hierarchy.superinterfaces(interfaces):
            if definition.name in self.hierarchy.client:
                add_implementations(table, definition)
        self._dispatch_tables[name] = table
        return table


def add_implementations(table, definition):
    for key, method in definition.methods.items():
        dispatched = not method.flags & (ACC_PRIVATE | ACC_STATIC)
        if dispatched and method.code_off and key[0][0] != "<" and key not in table:
            table[key] = (definition.name, key, method)


def load_classes(paths):
    classes = {}
    for path in paths:
        for dex in read_dexes(path):
            for definition in dex.classes():
                classes.setdefault(definition.name, definition)
    return classes


def read_manifest(apk):
    with zipfile.ZipFile(apk) as archive:
        if MANIFEST_ENTRY not in archive.namelist():
            raise ManifestError(f"{apk} has no {MANIFEST_ENTRY}")
        data = archive.read(MANIFEST_ENTRY)
    try:
        return read_components(data)
    except ManifestError as error:
        raise ManifestError(f"{apk}!{MANIFEST_ENTRY}: {error}") from error


def parse_entry(apk, entry, reader, data):
    try:
        return reader(data)
    except ResourceError as error:
        raise ResourceError(f"{apk}!{entry}: {error}") from error


def read_xml_resources(apks):
    documents = defaultdict(list)
    for apk in apks:
        with zipfile.ZipFile(apk) as archive:
            names = set(archive.namelist())
            if RESOURCE_TABLE_ENTRY not in names:
                continue
            table = parse_entry(
                apk,
                RESOURCE_TABLE_ENTRY,
                read_xml_files,
                archive.read(RESOURCE_TABLE_ENTRY),
            )
            for resource, paths in table.items():
                for path in paths:
                    if path not in names:
                        raise ResourceError(
                            f"{apk}!{RESOURCE_TABLE_ENTRY} names {path}, "
                            "which the APK lacks"
                        )
                    data = archive.read(path)
                    if is_compiled_xml(data):
                        documents[resource].append(
                            parse_entry(apk, path, read_compiled_xml, data)
                        )
    return documents


def native_class_names(apks):
    names = set()
    for apk in apks:
        with zipfile.ZipFile(apk) as archive:
            for entry in archive.namelist():
                if NATIVE_LIBRARY.fullmatch(entry):
                    names.update(
                        f"L{match[1].decode()};"
                        for match in NATIVE_CLASS_NAME.finditer(archive.read(entry))
                    )
    return names


def entry_points(apks):
    manifests = [read_manifest(apk) for apk in apks]
    applications = [found.application for found in manifests if found.application]
    launchers = [found.launcher for found in manifests if found.launcher]
    if not launchers:
        names = ", ".join(map(str, apks))
        raise ManifestError(f"no manifest in {names} declares a launcher activity")
    entries = [(applications[0], APPLICATION_ENTRY)] if applications else []
    entries.extend(
        (provider, PROVIDER_ENTRY)
        for found in manifests
        for provider in found.providers
    )
    entries.append((launchers[0], ACTIVITY_ENTRY))
    return entries


def gap_report(gap, users, reach, loaded):
    referenced_by = sorted(
        (
            {
                "class": name,
                "startup": any(
                    name in loaded if key is None else (name, key) in reach.methods
                    for key in keys
                ),
            }
            for name, keys in users.items()
        ),
        key=lambda user: (not user["startup"], user["class"]),
    )
    return {
        "id": gap.id,
        "kind": gap.kind.value,
        "startup": any(user["startup"] for user in referenced_by),
        "referenced_by": referenced_by,
    }


def scan(framework_dir, apks):
    framework = load_classes(sorted(Path(framework_dir).rglob("*.jar")))
    if OBJECT not in framework:
        raise DexError(
            f"{framework_dir} holds no java.lang.Object; pass the directory "
            "that holds api-impl.jar and art/"
        )
    client = load_classes(apks)
    if not client:
        raise DexError(f"no dex in {', '.join(map(str, apks))}")
    hierarchy = Hierarchy(framework, client)
    entries = entry_points(apks)
    for name, _ in entries:
        if name not in hierarchy.classes:
            raise DexError(
                f"the manifest names {name}, which no dex defines; "
                "pass every APK of the set"
            )
    references = GapFinder(hierarchy).find()
    reach = StartupReach(
        hierarchy, entries, native_class_names(apks), read_xml_resources(apks)
    )
    loaded = reach.loaded_classes()
    gaps = [gap_report(gap, users, reach, loaded) for gap, users in references.items()]
    gaps.sort(key=lambda gap: (not gap["startup"], gap["id"]))
    return {"entry_classes": [name for name, _ in entries], "gaps": gaps}


def count_line(gaps, label):
    counts = {kind: 0 for kind in GapKind}
    for gap in gaps:
        counts[GapKind(gap["kind"])] += 1
    breakdown = ", ".join(f"{kind.value} {counts[kind]}" for kind in GapKind)
    return f"{len(gaps)} {label}: {breakdown}"


def summary(result):
    gaps = result["gaps"]
    startup = [gap for gap in gaps if gap["startup"]]
    return "\n".join(
        (
            count_line(gaps, "framework gaps"),
            count_line(startup, "reachable at startup"),
        )
    )


def parse_arguments(argv):
    parser = argparse.ArgumentParser(
        description="List the android.* classes, methods and fields a client APK "
        "references that Eclipse's framework does not define."
    )
    parser.add_argument(
        "--framework",
        type=Path,
        required=True,
        metavar="DIR",
        help="Eclipse's framework directory, which holds api-impl.jar and art/",
    )
    parser.add_argument(
        "--json",
        type=Path,
        required=True,
        metavar="FILE",
        help="where to write every gap as JSON",
    )
    parser.add_argument(
        "apks",
        type=Path,
        nargs="+",
        metavar="APK",
        help="the client APK, or every APK of a split set",
    )
    return parser.parse_args(argv)


def main(argv):
    arguments = parse_arguments(argv)
    try:
        result = scan(arguments.framework, arguments.apks)
        arguments.json.write_text(json.dumps(result, indent=2) + "\n", "utf-8")
    except (
        OSError,
        zipfile.BadZipFile,
        DexError,
        ManifestError,
        ResourceError,
    ) as error:
        print(f"scan.py: {error}", file=sys.stderr)
        return 1
    print(summary(result))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
