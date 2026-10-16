import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from dex import (
    ACC_PRIVATE,
    ACC_STATIC,
    CHECK_CAST,
    CONST_CLASS,
    NEW_ARRAY,
    NEW_INSTANCE,
    SGET,
)
from fixtures import (
    ACC_ABSTRACT,
    ACC_INTERFACE,
    ACC_PUBLIC,
    INVOKE_DIRECT,
    INVOKE_INTERFACE,
    INVOKE_STATIC,
    INVOKE_SUPER,
    INVOKE_VIRTUAL,
    LAUNCHER_FILTER,
    OBJECT,
    RETURN_VOID,
    FixtureClass,
    FixtureMethod,
    build_dex,
    build_resource_table,
    build_xml,
    call,
    encoded_int,
    field,
    type_ref,
    write_archive,
)
from resources import ResourceError
from scan import scan, summary

SCRIPT = Path(__file__).resolve().with_name("scan.py")
RUNNABLE = "Ljava/lang/Runnable;"
CONTEXT = "Landroid/content/Context;"
APPLICATION = "Landroid/app/Application;"
ACTIVITY = "Landroid/app/Activity;"
PROVIDER = "Landroid/content/ContentProvider;"
VIEW_CONFIGURATION = "Landroid/view/ViewConfiguration;"
BUILD = "Landroid/os/Build;"
VERSION = "Landroid/os/Build$VERSION;"
TRACE = "Landroid/os/Trace;"
PROCESS = "Landroid/os/Process;"
LOCALE_MANAGER = "Landroid/app/LocaleManager;"
STRING = "Ljava/lang/String;"
APP = "Lcom/example/app/App;"
STARTUP = "Lcom/example/app/Startup;"
TASK = "Lcom/example/app/Task;"
HOLDER = "Lcom/example/app/Holder;"
REGISTRY = "Lcom/example/app/Registry;"
BASE = "Lcom/example/app/Base;"
CLICKER = "Lcom/example/app/Clicker;"
ON_CLICK = "Landroid/view/View$OnClickListener;"
COLUMNS = "Landroid/provider/BaseColumns;"
FANCY = "Lcom/example/app/Fancy;"
PLUGIN = "Lcom/example/app/Plugin;"
LISTENER = "Lcom/example/app/Listener;"
READY = "Lcom/example/app/Ready;"
IDLE = "Lcom/example/app/Idle;"
SPLASH = "Lcom/example/app/Splash;"
SETTINGS = "Lcom/example/app/Settings;"
LEGACY = "Lcom/example/app/Legacy;"
INIT_PROVIDER = "Lcom/example/app/InitProvider;"
SANDBOX_PROVIDER = "Lcom/example/app/SandboxProvider;"
COMPAT = "Landroid/support/Compat;"
MAIN = "Lcom/example/app/Main;"
HOOK = "Lcom/example/app/Hook;"
HOOKED = "Lcom/example/app/Hooked;"
ENGINE = "Lcom/example/app/Engine;"
WORKER = "Lcom/example/app/Worker;"
PARENT = "Lcom/example/app/Parent;"
CHILD = "Lcom/example/app/Child;"
GREETER = "Lcom/example/app/Greeter;"
POLITE = "Lcom/example/app/Polite;"
SHAPE = "Lcom/example/app/Shape;"
SQUARE = "Lcom/example/app/Square;"
BRIDGE = "Lcom/example/app/Bridge;"
DECOY = "Lcom/example/app/Decoy;"
LAYOUTS = "Lcom/example/app/R$layout;"
PROPERTIES = "Landroid/os/SystemProperties;"
VIEW_CONSTRUCTOR = "(Landroid/content/Context;Landroid/util/AttributeSet;)V"
MAIN_LAYOUT = 0x7F0C0001
BAR_LAYOUT = 0x7F0C0002
UNUSED_LAYOUT = 0x7F0C0003
RETURN = (RETURN_VOID,)


def method(name, proto, *code, flags=ACC_PUBLIC):
    return FixtureMethod(name, proto, code=(*code, RETURN), flags=flags)


def constructor(superclass=OBJECT):
    return method("<init>", "()V", call(INVOKE_DIRECT, superclass, "<init>", "()V"))


FRAMEWORK = (
    FixtureClass(
        OBJECT,
        superclass=None,
        methods=(method("<init>", "()V"), method("hashCode", "()I")),
    ),
    FixtureClass(
        RUNNABLE,
        flags=ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT,
        methods=(
            FixtureMethod("run", "()V", code=None, flags=ACC_PUBLIC | ACC_ABSTRACT),
        ),
    ),
    FixtureClass(
        CONTEXT,
        methods=(constructor(), method("getPackageName", f"(){STRING}")),
    ),
    FixtureClass(
        APPLICATION,
        superclass=CONTEXT,
        methods=(constructor(CONTEXT), method("onCreate", "()V")),
    ),
    FixtureClass(
        ACTIVITY,
        superclass=CONTEXT,
        methods=(
            constructor(CONTEXT),
            method("onCreate", "(Landroid/os/Bundle;)V"),
            method("onStart", "()V"),
            method("onDestroy", "()V"),
        ),
    ),
    FixtureClass(PROVIDER, methods=(constructor(), method("onCreate", "()Z"))),
    FixtureClass(
        VIEW_CONFIGURATION,
        methods=(method("getScaledTouchSlop", "()I"),),
    ),
    FixtureClass(BUILD, fields=(("MODEL", STRING),)),
    FixtureClass(VERSION, fields=(("SDK_INT", "I"),)),
    FixtureClass(
        ON_CLICK,
        flags=ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT,
        methods=(
            FixtureMethod(
                "onClick",
                "(Landroid/view/View;)V",
                code=None,
                flags=ACC_PUBLIC | ACC_ABSTRACT,
            ),
        ),
    ),
    FixtureClass(
        COLUMNS,
        flags=ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT,
        fields=(("_ID", STRING),),
    ),
    FixtureClass(PROCESS, methods=(method("myPid", "()I", flags=ACC_STATIC),)),
)

CLIENT = (
    FixtureClass(
        APP,
        superclass=APPLICATION,
        methods=(
            constructor(APPLICATION),
            method("onCreate", "()V", call(INVOKE_STATIC, STARTUP, "begin", "()V")),
        ),
    ),
    FixtureClass(
        STARTUP,
        methods=(
            method(
                "begin",
                "()V",
                call(INVOKE_STATIC, VIEW_CONFIGURATION, "getWindowTouchSlop", "()I"),
                call(INVOKE_STATIC, PROCESS, "myPid", "()I"),
                type_ref(NEW_INSTANCE, TASK),
                call(INVOKE_DIRECT, TASK, "<init>", "()V"),
                type_ref(NEW_INSTANCE, READY),
                call(INVOKE_DIRECT, READY, "<init>", "(I)V"),
                call(INVOKE_INTERFACE, LISTENER, "ready", "()V"),
                field(SGET, HOLDER, "VALUE", "I"),
                call(INVOKE_STATIC, REGISTRY, "register", "()V"),
                type_ref(CHECK_CAST, FANCY),
                call(INVOKE_VIRTUAL, CLICKER, "onClick", "(Landroid/view/View;)V"),
                field(SGET, CLICKER, "_ID", STRING),
                type_ref(CONST_CLASS, PLUGIN),
                type_ref(CONST_CLASS, "Landroidx/core/Missing;"),
                call(INVOKE_VIRTUAL, OBJECT, "missing", "()V"),
                call(INVOKE_STATIC, COMPAT, "help", "()V"),
                flags=ACC_PUBLIC | ACC_STATIC,
            ),
        ),
    ),
    FixtureClass(
        TASK,
        interfaces=(RUNNABLE,),
        methods=(
            method(
                "<clinit>",
                "()V",
                field(SGET, VERSION, "MEDIA_PERFORMANCE_CLASS", "I"),
                flags=ACC_STATIC,
            ),
            constructor(),
            method("run", "()V", field(SGET, BUILD, "SOC_MODEL", STRING)),
        ),
    ),
    FixtureClass(
        REGISTRY,
        methods=(
            method(
                "<clinit>",
                "()V",
                call(INVOKE_STATIC, PROCESS, "isApplicationUid", "(I)Z"),
                flags=ACC_STATIC,
            ),
            method("register", "()V", flags=ACC_STATIC),
        ),
    ),
    FixtureClass(BASE, superclass="Landroid/widget/Magnifier;"),
    FixtureClass(FANCY, superclass=BASE),
    FixtureClass(
        CLICKER,
        flags=ACC_PUBLIC | ACC_ABSTRACT,
        interfaces=(ON_CLICK, COLUMNS),
    ),
    FixtureClass(
        HOLDER,
        fields=(("VALUE", "I"),),
        methods=(
            method(
                "<clinit>",
                "()V",
                call(INVOKE_STATIC, TRACE, "beginSection", f"({STRING})V"),
                call(INVOKE_VIRTUAL, TRACE, "hashCode", "()I"),
                flags=ACC_STATIC,
            ),
        ),
    ),
    FixtureClass(
        PLUGIN,
        methods=(
            method(
                "<init>",
                "()V",
                type_ref(NEW_INSTANCE, LOCALE_MANAGER),
                call(INVOKE_DIRECT, LOCALE_MANAGER, "<init>", "()V"),
            ),
        ),
    ),
    FixtureClass(
        LISTENER,
        flags=ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT,
        methods=(
            FixtureMethod("ready", "()V", code=None, flags=ACC_PUBLIC | ACC_ABSTRACT),
        ),
    ),
    FixtureClass(
        READY,
        interfaces=(LISTENER,),
        methods=(
            method(
                "<init>",
                "()V",
                call(
                    INVOKE_STATIC, PROCESS, "myUserHandle", "()Landroid/os/UserHandle;"
                ),
            ),
            method("<init>", "(I)V", call(INVOKE_DIRECT, OBJECT, "<init>", "()V")),
            method("ready", "()V", call(INVOKE_STATIC, PROCESS, "isSdkSandbox", "()Z")),
        ),
    ),
    FixtureClass(
        IDLE,
        interfaces=(LISTENER,),
        methods=(
            constructor(),
            method("ready", "()V", call(INVOKE_STATIC, PROCESS, "isIsolated", "()Z")),
        ),
    ),
    FixtureClass(
        SPLASH,
        superclass=ACTIVITY,
        methods=(
            constructor(ACTIVITY),
            method(
                "onStart",
                "()V",
                call(
                    INVOKE_VIRTUAL,
                    SPLASH,
                    "getOnBackInvokedDispatcher",
                    "()Landroid/window/OnBackInvokedDispatcher;",
                ),
                call(INVOKE_VIRTUAL, SPLASH, "getPackageName", f"(){STRING}"),
            ),
            method(
                "onDestroy",
                "()V",
                call(INVOKE_VIRTUAL, VIEW_CONFIGURATION, "getScaledHoverSlop", "()I"),
            ),
        ),
    ),
    FixtureClass(
        SETTINGS,
        superclass=ACTIVITY,
        methods=(
            method(
                "onCreate",
                "(Landroid/os/Bundle;)V",
                type_ref(CHECK_CAST, "Landroid/app/DialogFragment;"),
            ),
        ),
    ),
    FixtureClass(LEGACY, superclass="Landroid/app/DialogFragment;"),
    FixtureClass(
        INIT_PROVIDER,
        superclass=PROVIDER,
        methods=(method("onCreate", "()Z", field(SGET, BUILD, "SKU", STRING)),),
    ),
    FixtureClass(
        SANDBOX_PROVIDER,
        superclass=PROVIDER,
        methods=(method("onCreate", "()Z", field(SGET, BUILD, "ODM_SKU", STRING)),),
    ),
    FixtureClass(COMPAT, methods=(method("help", "()V", flags=ACC_STATIC),)),
)

MANIFEST = (
    "manifest",
    {"package": "com.example.app"},
    (
        (
            "application",
            {"android:name": ".App"},
            (
                ("provider", {"android:name": ".InitProvider"}, ()),
                (
                    "provider",
                    {"android:name": ".SandboxProvider", "android:process": ":box"},
                    (),
                ),
                ("activity", {"android:name": ".Settings"}, ()),
                ("activity", {"android:name": ".Splash"}, (LAUNCHER_FILTER,)),
            ),
        ),
    ),
)


def build_field(name):
    return field(SGET, BUILD, name, STRING)


def build_gap(name):
    return f"{BUILD}->{name}:{STRING}"


def interface(name, *methods):
    return FixtureClass(
        name, flags=ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT, methods=methods
    )


def view(name, gap):
    return FixtureClass(
        name, methods=(method("<init>", VIEW_CONSTRUCTOR, build_field(gap)),)
    )


def dex_entry(number):
    return "classes.dex" if number == 0 else f"classes{number + 1}.dex"


MAIN_MANIFEST = (
    "manifest",
    {"package": "com.example.app"},
    (
        (
            "application",
            {},
            (("activity", {"android:name": ".Main"}, (LAUNCHER_FILTER,)),),
        ),
    ),
)


def user(name, startup):
    return {"class": name, "startup": startup}


EXPECTED_GAPS = [
    {
        "id": "Landroid/app/Activity;->getOnBackInvokedDispatcher()"
        "Landroid/window/OnBackInvokedDispatcher;",
        "kind": "method",
        "startup": True,
        "referenced_by": [user(SPLASH, True)],
    },
    {
        "id": LOCALE_MANAGER,
        "kind": "class",
        "startup": True,
        "referenced_by": [user(PLUGIN, True)],
    },
    {
        "id": f"{LOCALE_MANAGER}-><init>()V",
        "kind": "method",
        "startup": True,
        "referenced_by": [user(PLUGIN, True)],
    },
    {
        "id": "Landroid/os/Build$VERSION;->MEDIA_PERFORMANCE_CLASS:I",
        "kind": "field",
        "startup": True,
        "referenced_by": [user(TASK, True)],
    },
    {
        "id": "Landroid/os/Build;->SKU:Ljava/lang/String;",
        "kind": "field",
        "startup": True,
        "referenced_by": [user(INIT_PROVIDER, True)],
    },
    {
        "id": "Landroid/os/Build;->SOC_MODEL:Ljava/lang/String;",
        "kind": "field",
        "startup": True,
        "referenced_by": [user(TASK, True)],
    },
    {
        "id": "Landroid/os/Process;->isApplicationUid(I)Z",
        "kind": "method",
        "startup": True,
        "referenced_by": [user(REGISTRY, True)],
    },
    {
        "id": "Landroid/os/Process;->isSdkSandbox()Z",
        "kind": "method",
        "startup": True,
        "referenced_by": [user(READY, True)],
    },
    {
        "id": TRACE,
        "kind": "class",
        "startup": True,
        "referenced_by": [user(HOLDER, True)],
    },
    {
        "id": f"{TRACE}->beginSection({STRING})V",
        "kind": "method",
        "startup": True,
        "referenced_by": [user(HOLDER, True)],
    },
    {
        "id": "Landroid/view/ViewConfiguration;->getWindowTouchSlop()I",
        "kind": "method",
        "startup": True,
        "referenced_by": [user(STARTUP, True)],
    },
    {
        "id": "Landroid/widget/Magnifier;",
        "kind": "class",
        "startup": True,
        "referenced_by": [user(BASE, True)],
    },
    {
        "id": "Landroid/app/DialogFragment;",
        "kind": "class",
        "startup": False,
        "referenced_by": [user(LEGACY, False), user(SETTINGS, False)],
    },
    {
        "id": "Landroid/os/Build;->ODM_SKU:Ljava/lang/String;",
        "kind": "field",
        "startup": False,
        "referenced_by": [user(SANDBOX_PROVIDER, False)],
    },
    {
        "id": "Landroid/os/Process;->isIsolated()Z",
        "kind": "method",
        "startup": False,
        "referenced_by": [user(IDLE, False)],
    },
    {
        "id": "Landroid/os/Process;->myUserHandle()Landroid/os/UserHandle;",
        "kind": "method",
        "startup": False,
        "referenced_by": [user(READY, False)],
    },
    {
        "id": "Landroid/view/ViewConfiguration;->getScaledHoverSlop()I",
        "kind": "method",
        "startup": False,
        "referenced_by": [user(SPLASH, False)],
    },
]


class ScanTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.framework = self.root / "framework"
        (self.framework / "art").mkdir(parents=True)
        write_archive(
            self.framework / "art" / "core-oj-hostdex.jar",
            {"classes.dex": build_dex(FRAMEWORK[:2])},
        )
        write_archive(
            self.framework / "api-impl.jar",
            {"classes.dex": build_dex(FRAMEWORK[2:])},
        )
        self.apk = write_archive(
            self.root / "base.apk",
            {
                "AndroidManifest.xml": build_xml(MANIFEST),
                "classes.dex": build_dex(CLIENT[:6]),
                "classes2.dex": build_dex(CLIENT[6:]),
            },
        )
        self.split = write_archive(
            self.root / "split_config.x86_64.apk",
            {
                "AndroidManifest.xml": build_xml(
                    ("manifest", {"package": "com.example.app"}, ())
                )
            },
        )

    def run_script(self, *arguments):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *map(str, arguments)],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_lists_every_gap_with_its_startup_reach(self):
        result = scan(self.framework, [self.apk, self.split])
        self.assertEqual(result["entry_classes"], [APP, INIT_PROVIDER, SPLASH])
        self.assertEqual(result["gaps"], EXPECTED_GAPS)

    def test_summary_counts_gaps_and_startup_gaps(self):
        self.assertEqual(
            summary(scan(self.framework, [self.apk])),
            "17 framework gaps: class 4, method 9, field 4\n"
            "12 reachable at startup: class 3, method 6, field 3",
        )

    def test_command_writes_json_and_prints_summary(self):
        output = self.root / "gaps.json"
        finished = self.run_script(
            "--framework", self.framework, "--json", output, self.apk, self.split
        )
        self.assertEqual(finished.returncode, 0, finished.stderr)
        self.assertEqual(
            finished.stdout,
            "17 framework gaps: class 4, method 9, field 4\n"
            "12 reachable at startup: class 3, method 6, field 3\n",
        )
        self.assertEqual(json.loads(output.read_text())["gaps"], EXPECTED_GAPS)

    def test_command_rejects_a_directory_without_the_core_library(self):
        (self.framework / "art" / "core-oj-hostdex.jar").unlink()
        finished = self.run_script(
            "--framework", self.framework, "--json", self.root / "gaps.json", self.apk
        )
        self.assertEqual(finished.returncode, 1)
        self.assertIn("holds no java.lang.Object", finished.stderr)
        self.assertFalse((self.root / "gaps.json").exists())

    def test_command_needs_a_launcher_activity(self):
        headless = write_archive(
            self.root / "headless.apk",
            {
                "AndroidManifest.xml": build_xml(
                    ("manifest", {"package": "com.example.app"}, ())
                ),
                "classes.dex": build_dex(CLIENT),
            },
        )
        finished = self.run_script(
            "--framework", self.framework, "--json", self.root / "gaps.json", headless
        )
        self.assertEqual(finished.returncode, 1)
        self.assertIn("declares a launcher activity", finished.stderr)


class ReachTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)

    def scan_main(self, on_create, classes=(), framework=(FRAMEWORK[2:],), entries=()):
        framework_dir = self.root / "framework"
        (framework_dir / "art").mkdir(parents=True)
        write_archive(
            framework_dir / "art" / "core-oj-hostdex.jar",
            {"classes.dex": build_dex(FRAMEWORK[:2])},
        )
        write_archive(
            framework_dir / "api-impl.jar",
            {
                dex_entry(number): build_dex(group)
                for number, group in enumerate(framework)
            },
        )
        main = FixtureClass(
            MAIN,
            superclass=ACTIVITY,
            methods=(
                constructor(ACTIVITY),
                method("onCreate", "(Landroid/os/Bundle;)V", *on_create),
            ),
        )
        apk = write_archive(
            self.root / "app.apk",
            {
                "AndroidManifest.xml": build_xml(MAIN_MANIFEST),
                "classes.dex": build_dex((main, *classes)),
                **dict(entries),
            },
        )
        return scan(framework_dir, [apk])

    def startup_gaps(self, on_create, **arguments):
        result = self.scan_main(on_create, **arguments)
        return [(gap["id"], gap["startup"]) for gap in result["gaps"]]

    def test_a_call_site_before_new_instance_reaches_the_implementation(self):
        gaps = self.startup_gaps(
            (
                call(INVOKE_INTERFACE, HOOK, "fire", "()V"),
                type_ref(NEW_INSTANCE, HOOKED),
                call(INVOKE_DIRECT, HOOKED, "<init>", "()V"),
            ),
            classes=(
                interface(
                    HOOK,
                    FixtureMethod(
                        "fire", "()V", code=None, flags=ACC_PUBLIC | ACC_ABSTRACT
                    ),
                ),
                FixtureClass(
                    HOOKED,
                    interfaces=(HOOK,),
                    methods=(
                        constructor(),
                        method("fire", "()V", build_field("SKU")),
                    ),
                ),
            ),
        )
        self.assertEqual(gaps, [(build_gap("SKU"), True)])

    def test_direct_and_super_calls_reach_client_code(self):
        gaps = self.startup_gaps(
            (
                type_ref(NEW_INSTANCE, WORKER),
                call(INVOKE_DIRECT, WORKER, "<init>", "(I)V"),
            ),
            classes=(
                FixtureClass(
                    ENGINE,
                    interfaces=(RUNNABLE,),
                    methods=(
                        method("<init>", "()V", build_field("SKU")),
                        method("run", "()V", build_field("ODM_SKU")),
                    ),
                ),
                FixtureClass(
                    WORKER,
                    superclass=ENGINE,
                    methods=(
                        method(
                            "<init>",
                            "(I)V",
                            call(INVOKE_DIRECT, ENGINE, "<init>", "()V"),
                            call(INVOKE_DIRECT, WORKER, "prepare", "()V"),
                        ),
                        method(
                            "prepare",
                            "()V",
                            build_field("SOC_MODEL"),
                            flags=ACC_PRIVATE,
                        ),
                        method("run", "()V", call(INVOKE_SUPER, ENGINE, "run", "()V")),
                    ),
                ),
            ),
        )
        self.assertEqual(
            gaps,
            [
                (build_gap("ODM_SKU"), True),
                (build_gap("SKU"), True),
                (build_gap("SOC_MODEL"), True),
            ],
        )

    def test_the_first_dex_of_a_framework_jar_wins(self):
        patched = FixtureClass(
            PROPERTIES,
            methods=(method("get", f"({STRING}){STRING}", flags=ACC_STATIC),),
        )
        stock = FixtureClass(
            PROPERTIES,
            methods=(method("getInt", f"({STRING}I)I", flags=ACC_STATIC),),
        )
        gaps = self.startup_gaps(
            (
                call(INVOKE_STATIC, PROPERTIES, "get", f"({STRING}){STRING}"),
                call(INVOKE_STATIC, PROPERTIES, "getInt", f"({STRING}I)I"),
            ),
            framework=((*FRAMEWORK[2:], patched), (stock,)),
        )
        self.assertEqual(gaps, [(f"{PROPERTIES}->getInt({STRING}I)I", True)])

    def test_framework_classes_shadow_client_copies(self):
        bundled = FixtureClass(
            BUILD,
            fields=(("MODEL", STRING), ("SERIAL", STRING)),
            methods=(
                method(
                    "describe",
                    "()V",
                    call(
                        INVOKE_STATIC, VIEW_CONFIGURATION, "getWindowTouchSlop", "()I"
                    ),
                    flags=ACC_STATIC,
                ),
            ),
        )
        gaps = self.startup_gaps((build_field("SERIAL"),), classes=(bundled,))
        self.assertEqual(gaps, [(build_gap("SERIAL"), True)])

    def test_initializing_a_class_initializes_its_client_superclasses(self):
        gaps = self.startup_gaps(
            (call(INVOKE_STATIC, CHILD, "make", "()V"),),
            classes=(
                FixtureClass(
                    PARENT,
                    methods=(
                        method("<clinit>", "()V", build_field("SKU"), flags=ACC_STATIC),
                    ),
                ),
                FixtureClass(
                    CHILD,
                    superclass=PARENT,
                    methods=(method("make", "()V", flags=ACC_STATIC),),
                ),
            ),
        )
        self.assertEqual(gaps, [(build_gap("SKU"), True)])

    def test_default_methods_of_client_interfaces_take_calls(self):
        gaps = self.startup_gaps(
            (
                type_ref(NEW_INSTANCE, POLITE),
                call(INVOKE_DIRECT, POLITE, "<init>", "()V"),
                call(INVOKE_INTERFACE, GREETER, "greet", "()V"),
            ),
            classes=(
                interface(GREETER, method("greet", "()V", build_field("SKU"))),
                FixtureClass(POLITE, interfaces=(GREETER,), methods=(constructor(),)),
            ),
        )
        self.assertEqual(gaps, [(build_gap("SKU"), True)])

    def test_private_methods_do_not_take_virtual_calls(self):
        gaps = self.startup_gaps(
            (
                type_ref(NEW_INSTANCE, SQUARE),
                call(INVOKE_DIRECT, SQUARE, "<init>", "()V"),
                call(INVOKE_VIRTUAL, SHAPE, "draw", "()V"),
            ),
            classes=(
                FixtureClass(
                    SHAPE,
                    methods=(constructor(), method("draw", "()V", build_field("SKU"))),
                ),
                FixtureClass(
                    SQUARE,
                    superclass=SHAPE,
                    methods=(
                        constructor(SHAPE),
                        method(
                            "draw", "()V", build_field("ODM_SKU"), flags=ACC_PRIVATE
                        ),
                    ),
                ),
            ),
        )
        self.assertEqual(
            gaps, [(build_gap("SKU"), True), (build_gap("ODM_SKU"), False)]
        )

    def test_an_array_of_a_missing_class_is_a_class_gap(self):
        gaps = self.startup_gaps(
            (type_ref(NEW_ARRAY, "[Landroid/media/AudioPlaybackConfiguration;"),)
        )
        self.assertEqual(gaps, [("Landroid/media/AudioPlaybackConfiguration;", True)])

    def test_classes_that_native_libraries_name_are_reached(self):
        gaps = self.startup_gaps(
            (),
            classes=(
                FixtureClass(
                    BRIDGE,
                    methods=(
                        method("onEvent", "()V", build_field("SKU"), flags=ACC_PRIVATE),
                    ),
                ),
                FixtureClass(
                    DECOY,
                    methods=(
                        method("run", "()V", build_field("ODM_SKU"), flags=ACC_STATIC),
                    ),
                ),
            ),
            entries=(
                (
                    "lib/x86_64/libgame.so",
                    b"\x7fELF\0com/example/app/Bridge\0(Lcom/example/app/Decoy;)V\0",
                ),
                ("assets/names.bin", b"\0com/example/app/Decoy\0"),
            ),
        )
        self.assertEqual(
            gaps, [(build_gap("SKU"), True), (build_gap("ODM_SKU"), False)]
        )

    def test_views_of_layouts_that_startup_code_reads_are_constructed(self):
        layouts = FixtureClass(
            LAYOUTS,
            fields=(
                ("main", "I", encoded_int(MAIN_LAYOUT)),
                ("unused", "I", encoded_int(UNUSED_LAYOUT)),
            ),
        )
        gaps = self.startup_gaps(
            (field(SGET, LAYOUTS, "main", "I"),),
            classes=(
                layouts,
                view("Lcom/example/app/GameView;", "SKU"),
                view("Lcom/example/app/WideView;", "SOC_MODEL"),
                view("Lcom/example/app/Bar;", "ODM_SKU"),
                view("Lcom/example/app/Lonely;", "SOC_MANUFACTURER"),
            ),
            entries=(
                (
                    "resources.arsc",
                    build_resource_table(
                        (
                            (MAIN_LAYOUT, "res/layout/main.xml"),
                            (MAIN_LAYOUT, "res/layout-land/main.xml"),
                            (BAR_LAYOUT, "res/layout/bar.xml"),
                            (UNUSED_LAYOUT, "res/layout/unused.xml"),
                            (0x7F0E0001, "res/raw/notes.xml"),
                        )
                    ),
                ),
                (
                    "res/layout/main.xml",
                    build_xml(
                        (
                            "FrameLayout",
                            {},
                            (
                                ("com.example.app.GameView", {}, ()),
                                ("include", {"layout": BAR_LAYOUT}, ()),
                            ),
                        )
                    ),
                ),
                (
                    "res/layout-land/main.xml",
                    build_xml(("com.example.app.WideView", {}, ())),
                ),
                ("res/layout/bar.xml", build_xml(("com.example.app.Bar", {}, ()))),
                (
                    "res/layout/unused.xml",
                    build_xml(("com.example.app.Lonely", {}, ())),
                ),
                ("res/raw/notes.xml", b"<notes/>"),
            ),
        )
        self.assertEqual(
            gaps,
            [
                (build_gap("ODM_SKU"), True),
                (build_gap("SKU"), True),
                (build_gap("SOC_MODEL"), True),
                (build_gap("SOC_MANUFACTURER"), False),
            ],
        )

    def test_a_resource_table_must_not_name_absent_files(self):
        table = build_resource_table(((MAIN_LAYOUT, "res/layout/main.xml"),))
        with self.assertRaisesRegex(ResourceError, "names res/layout/main.xml"):
            self.scan_main((), entries=(("resources.arsc", table),))


if __name__ == "__main__":
    unittest.main()
