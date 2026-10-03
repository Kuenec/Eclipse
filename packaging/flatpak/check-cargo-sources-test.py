#!/usr/bin/env python3

import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().with_name("check-cargo-sources.py")
CHECK = Path("packaging/flatpak/check-cargo-sources.py")
SOURCES = Path("packaging/flatpak/cargo-sources.json")
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"


def checksum(crate):
    return hashlib.sha256(crate.encode()).hexdigest()


def vendored(crate):
    return [
        {
            "type": "archive",
            "archive-type": "tar-gzip",
            "url": f"https://static.crates.io/crates/{crate}.crate",
            "sha256": checksum(crate),
            "dest": f"cargo/vendor/{crate}",
        },
        {
            "type": "inline",
            "contents": json.dumps({"package": checksum(crate), "files": {}}),
            "dest": f"cargo/vendor/{crate}",
            "dest-filename": ".cargo-checksum.json",
        },
    ]


class CheckCargoSourcesTest(unittest.TestCase):
    def setUp(self):
        self.repo = Path(self.enterContext(tempfile.TemporaryDirectory()))
        (self.repo / CHECK).parent.mkdir(parents=True)
        shutil.copy(SCRIPT, self.repo / CHECK)
        self.git("init", "--quiet", "--initial-branch=main")

    def git(self, *arguments):
        subprocess.run(["git", "-C", self.repo, *arguments], check=True)

    def vendor(self, *crates):
        sources = [source for crate in crates for source in vendored(crate)]
        sources.append(
            {
                "type": "inline",
                "contents": "",
                "dest": "cargo",
                "dest-filename": "config.toml",
            }
        )
        (self.repo / SOURCES).write_text(json.dumps(sources), encoding="utf-8")

    def lock(self, path, *crates):
        packages = []
        for crate in crates:
            name, version = crate.rsplit("-", 1)
            packages.append(
                f'[[package]]\nname = "{name}"\nversion = "{version}"\n'
                f'source = "{CRATES_IO}"\nchecksum = "{checksum(crate)}"\n'
            )
        lockfile = self.repo / path
        lockfile.parent.mkdir(parents=True, exist_ok=True)
        lockfile.write_text("version = 4\n\n" + "\n".join(packages), encoding="utf-8")

    def check(self):
        return subprocess.run(
            [sys.executable, self.repo / CHECK],
            capture_output=True,
            encoding="utf-8",
            check=False,
        )

    def test_every_tracked_lockfile_counts(self):
        self.vendor("alpha-1.0.0", "beta-2.0.0")
        self.lock("Cargo.lock", "alpha-1.0.0")
        self.lock("crates/helper/Cargo.lock", "beta-2.0.0")
        self.git("add", "Cargo.lock", "crates/helper/Cargo.lock")
        run = self.check()
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("vendors all 2 locked crates", run.stdout)

    def test_a_staged_lockfile_with_an_unvendored_crate_fails_naming_it(self):
        self.vendor("alpha-1.0.0")
        self.lock("Cargo.lock", "alpha-1.0.0")
        self.lock("crates/settings/Cargo.lock", "gamma-3.0.0")
        self.git("add", "Cargo.lock", "crates/settings/Cargo.lock")
        run = self.check()
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertEqual(run.stdout, "")
        self.assertIn("gamma-3.0.0: no cargo/vendor/gamma-3.0.0 archive", run.stderr)
        self.assertNotIn("alpha-1.0.0", run.stderr)

    def test_an_untracked_lockfile_does_not_count(self):
        self.vendor("alpha-1.0.0")
        self.lock("Cargo.lock", "alpha-1.0.0")
        self.lock("build/scratch/Cargo.lock", "gamma-3.0.0")
        self.git("add", "Cargo.lock")
        run = self.check()
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("vendors all 1 locked crates", run.stdout)

    def test_a_repository_without_a_tracked_lockfile_fails(self):
        self.vendor()
        self.lock("Cargo.lock", "alpha-1.0.0")
        run = self.check()
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("git tracks no Cargo.lock", run.stderr)


if __name__ == "__main__":
    unittest.main()
