import struct
from dataclasses import dataclass, field

from resources import (
    RES_STRING_POOL_TYPE,
    RES_XML_END_ELEMENT_TYPE,
    RES_XML_START_ELEMENT_TYPE,
    RES_XML_TYPE,
    TYPE_STRING,
    ResourceError,
    StringPool,
    document_chunks,
    start_element,
)

ANDROID_NS = "http://schemas.android.com/apk/res/android"
MAIN_ACTION = "android.intent.action.MAIN"
LAUNCHER_CATEGORY = "android.intent.category.LAUNCHER"


class ManifestError(Exception):
    pass


@dataclass(frozen=True)
class Components:
    application: str | None
    providers: tuple
    launcher: str | None


@dataclass
class Element:
    tag: str
    activity: str | None = None
    actions: set = field(default_factory=set)
    categories: set = field(default_factory=set)


def class_descriptor(package, name):
    if name.startswith("."):
        name = package + name
    elif "." not in name:
        name = f"{package}.{name}"
    return f"L{name.replace('.', '/')};"


def read_components(data):
    try:
        return parse_components(data)
    except (struct.error, IndexError, UnicodeDecodeError, ResourceError) as error:
        raise ManifestError(f"malformed binary XML: {error}") from error


def parse_components(data):
    pool = None
    package = None
    application = None
    providers = []
    launcher = None
    stack = []
    for kind, pos in document_chunks(data, RES_XML_TYPE):
        if kind == RES_STRING_POOL_TYPE and pool is None:
            pool = StringPool(data, pos)
        elif kind == RES_XML_START_ELEMENT_TYPE:
            if pool is None:
                raise ManifestError("an element precedes the string pool")
            tag, attributes = start_element(data, pos, pool)
            values = {
                (pool.get(namespace), pool.get(key)): pool.get(value)
                for namespace, key, value_type, value in attributes
                if value_type == TYPE_STRING
            }
            name = values.get((ANDROID_NS, "name"))
            parent = stack[-1] if stack else None
            element = Element(tag)
            if tag == "manifest":
                package = values.get((None, "package"))
            elif tag == "application":
                application = name
            elif tag == "provider" and name:
                process = values.get((ANDROID_NS, "process")) or ""
                if ":" not in process:
                    providers.append(name)
            elif tag in ("activity", "activity-alias"):
                element.activity = values.get((ANDROID_NS, "targetActivity"), name)
            elif tag == "action" and parent and name:
                parent.actions.add(name)
            elif tag == "category" and parent and name:
                parent.categories.add(name)
            stack.append(element)
        elif kind == RES_XML_END_ELEMENT_TYPE:
            if not stack:
                raise ManifestError("an element closes that never opened")
            element = stack.pop()
            if (
                element.tag == "intent-filter"
                and stack
                and stack[-1].activity
                and launcher is None
                and MAIN_ACTION in element.actions
                and LAUNCHER_CATEGORY in element.categories
            ):
                launcher = stack[-1].activity
    if package is None:
        raise ManifestError("the manifest declares no package")

    def descriptor(name):
        return None if name is None else class_descriptor(package, name)

    return Components(
        descriptor(application),
        tuple(map(descriptor, providers)),
        descriptor(launcher),
    )
