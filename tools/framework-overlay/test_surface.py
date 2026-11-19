import tempfile
import unittest
from pathlib import Path

from surface import Framework, SurfaceError, add_missing, read_members

EFFECT = "Landroid/os/VibrationEffect;"
VIEW = "Landroid/view/View;"
BUTTON = "Landroid/widget/Button;"
CALLBACK = "Landroid/view/Window$Callback;"
BUILD = "Landroid/os/Build;"
CONSTRUCTOR = """
.method public constructor <init>()V
    .registers 1

    return-void
.end method
"""


def class_smali(name, superclass="Ljava/lang/Object;", flags="public", body=""):
    return f".class {flags} {name}\n.super {superclass}\n{body}"


class SurfaceTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        self.view, self.stock, self.fixed = root / "view", root / "stock", root / "fixed"
        self.framework = Framework(self.view, self.stock, self.fixed)

    def write(self, root, name, **arguments):
        file = root / f"{name[1:-1]}.smali"
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(class_smali(name, **arguments), "utf-8")
        return file

    def add(self, *lines):
        return add_missing(read_members("\n".join(lines)), self.framework)

    def test_a_missing_method_reports_its_use_and_returns_the_zero_of_its_type(self):
        effect = self.write(self.view, EFFECT)
        outcomes, skipped = self.add(
            f"method static {EFFECT}->createWaveform([J[II){EFFECT}",
            f"method instance {EFFECT}->getDuration(JD)J",
            f"method instance {EFFECT}->describeContents()I",
            f"method instance {EFFECT}->cancel()V",
        )
        self.assertEqual((outcomes["added"], skipped), (4, []))
        text = effect.read_text("utf-8")
        self.assertIn(
            f".method public static createWaveform([J[II){EFFECT}\n"
            "    .registers 5\n\n"
            f'    const-string v0, "{EFFECT}->createWaveform([J[II){EFFECT}"\n\n'
            "    invoke-static {v0}, Landroid/util/EclipseMissingApi;->used(Ljava/lang/String;)V\n\n"
            "    const/4 v0, 0x0\n\n"
            "    return-object v0\n"
            ".end method\n",
            text,
        )
        self.assertIn(".method public getDuration(JD)J\n    .registers 7\n", text)
        self.assertIn("    const-wide/16 v0, 0x0\n\n    return-wide v0\n", text)
        self.assertIn("    const/4 v0, 0x0\n\n    return v0\n", text)
        self.assertIn(".method public cancel()V\n    .registers 3\n", text)
        self.assertIn("    return-void\n", text)

    def test_a_stock_class_changes_only_as_a_copy_in_the_view(self):
        stock = self.write(self.stock, VIEW)
        outcomes, _ = self.add(
            f"field static {VIEW}->TRANSLATION_Z:Landroid/util/Property;",
            f"field instance {VIEW}->mAttachInfo:Ljava/lang/Object;",
        )
        self.assertEqual(outcomes["added"], 2)
        self.assertEqual(stock.read_text("utf-8"), class_smali(VIEW))
        self.assertEqual(
            (self.view / "android/view/View.smali").read_text("utf-8"),
            class_smali(VIEW)
            + "\n.field public static TRANSLATION_Z:Landroid/util/Property;\n"
            + "\n.field public mAttachInfo:Ljava/lang/Object;\n",
        )

    def test_defined_members_and_undefined_classes_are_left_alone(self):
        self.write(self.view, EFFECT, body=".method public static cancel()V\n.end method\n")
        outcomes, skipped = self.add(
            f"method static {EFFECT}->cancel()V",
            "method instance Landroid/app/Missing;->run()V",
        )
        self.assertEqual((outcomes["added"], outcomes["defined"]), (0, 1))
        self.assertEqual(
            [(member.owner, reason) for member, reason in skipped],
            [("Landroid/app/Missing;", "the framework does not define its class")],
        )

    def test_a_class_of_the_first_dex_changes_in_place_because_it_wins(self):
        first = self.write(self.fixed, BUILD)
        shadowed = self.write(self.view, BUILD)
        self.write(self.stock, BUILD)
        outcomes, _ = self.add(f"field static {BUILD}->SOC_MODEL:Ljava/lang/String;")
        self.assertEqual(outcomes["added"], 1)
        self.assertIn(".field public static SOC_MODEL:Ljava/lang/String;", first.read_text("utf-8"))
        self.assertEqual(shadowed.read_text("utf-8"), class_smali(BUILD))

    def test_a_constructor_builds_on_one_without_arguments(self):
        own = self.write(self.view, EFFECT, body=CONSTRUCTOR)
        self.write(self.stock, VIEW, body=CONSTRUCTOR)
        inherited = self.write(self.view, BUTTON, superclass=VIEW)
        self.write(self.stock, "Landroid/os/Handle;", superclass=EFFECT.replace("Effect", "None"))
        outcomes, skipped = self.add(
            f"method instance {EFFECT}-><init>(I)V",
            f"method instance {BUTTON}-><init>(Landroid/content/Context;)V",
            "method instance Landroid/os/Handle;-><init>(J)V",
        )
        self.assertEqual(outcomes["added"], 2)
        self.assertEqual(
            [reason for _, reason in skipped], ["no constructor without arguments to build on"]
        )
        self.assertIn(
            ".method public constructor <init>(I)V\n    .registers 4\n\n"
            f"    invoke-direct {{p0}}, {EFFECT}-><init>()V\n",
            own.read_text("utf-8"),
        )
        self.assertIn(f"    invoke-direct {{p0}}, {VIEW}-><init>()V\n", inherited.read_text("utf-8"))

    def test_interfaces_get_default_methods_and_constant_fields(self):
        callback = self.write(self.stock, CALLBACK, flags="public interface abstract")
        self.write(self.stock, "Landroid/annotation/Mark;", flags="public interface abstract annotation")
        outcomes, skipped = self.add(
            f"method instance {CALLBACK}->onSearchRequested()Z",
            f"field static {CALLBACK}->NONE:I",
            f"field instance {CALLBACK}->state:I",
            "method instance Landroid/annotation/Mark;->value()I",
        )
        self.assertEqual(outcomes["added"], 2)
        self.assertEqual(
            [reason for _, reason in skipped],
            ["an interface has no instance fields", "its class is an annotation"],
        )
        text = (self.view / "android/view/Window$Callback.smali").read_text("utf-8")
        self.assertIn(".method public onSearchRequested()Z\n    .registers 3\n", text)
        self.assertIn(".field public static final NONE:I\n", text)
        self.assertEqual(callback.read_text("utf-8").count(".method"), 0)

    def test_a_line_that_names_no_member_is_an_error(self):
        for line in (
            "class Landroid/app/Person;",
            f"method virtual {EFFECT}->cancel()V",
            f"method static {EFFECT}->cancel",
            f"field static {EFFECT}->EFFECT_CLICK",
        ):
            with self.assertRaises(SurfaceError, msg=line):
                read_members(line)


if __name__ == "__main__":
    unittest.main()
