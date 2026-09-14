"""Offline packaging regressions using temporary, synthetic Cargo sources."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location(
    "collect_licenses", Path(__file__).resolve().parents[2] / "scripts/collect_licenses.py"
)
notices = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(notices)
TARGET = "aarch64-apple-darwin"
MIT = b"""MIT License

Copyright (c) 2026 Synthetic Contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"""


class LicenseNoticeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="memq-license-test-")
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name)
        self.repo = self.base / "project"
        self.repo.mkdir()
        for name in ("Cargo.toml", "Cargo.lock"):
            (self.repo / name).write_text("# synthetic fixture\n", encoding="utf-8")
        self.package = self.make_package("fixture")
        self.crate = Path(self.package["manifest_path"]).parent

    def make_package(self, name):
        root = self.base / "sources" / name
        root.mkdir(parents=True)
        (root / "Cargo.toml").write_text("# synthetic fixture\n", encoding="utf-8")
        return {
            "id": "path+file://" + root.as_posix(),
            "name": name,
            "version": "1.0.0",
            "license": "MIT",
            "license_file": None,
            "manifest_path": str(root / "Cargo.toml"),
            "authors": ["SYNTHETIC_OWNER_METADATA"],
            "metadata": {"private": "SYNTHETIC_METADATA_DO_NOT_PUBLISH"},
            "source": None,
        }

    def put_notice(self, relative="LICENSE", data=MIT):
        path = self.crate / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return path

    def collect(self, packages=None):
        packages = [self.package] if packages is None else packages
        root = "synthetic-root"
        metadata = {
            "packages": [{"id": root}] + packages,
            "workspace_members": [root],
            "resolve": {
                "root": root,
                "nodes": [
                    {"id": root, "features": [], "deps": [
                        {"pkg": p["id"], "dep_kinds": [{"kind": None}]} for p in packages
                    ]}
                ] + [{"id": p["id"], "features": [], "deps": []} for p in packages],
            },
        }
        result = subprocess.CompletedProcess([], 0, json.dumps(metadata), "")
        # Mock the process boundary; all parsing, source reads and packaging are real.
        with mock.patch.object(notices.subprocess, "run", return_value=result):
            return notices.collect(self.repo, TARGET)

    @staticmethod
    def snapshot(directory):
        return {p.relative_to(directory).as_posix(): p.read_bytes()
                for p in directory.rglob("*") if p.is_file()}

    def test_absolute_and_traversing_license_file_paths_are_rejected(self):
        inside = self.put_notice()
        outside = self.crate.parent / "outside-license"
        outside.write_bytes(MIT)
        (self.crate / "legal").mkdir()
        paths = (
            str(inside.resolve()), str(outside.resolve()), "../outside-license",
            "legal/../LICENSE", r"C:\synthetic\LICENSE", r"..\outside-license",
            r"\\synthetic-host\share\LICENSE",
        )
        for path in paths:
            with self.subTest(path=path):
                self.package["license_file"] = path
                with self.assertRaises(notices.NoticeError):
                    self.collect()

    def test_discovered_unsafe_file_and_directory_symlinks_are_rejected(self):
        inside = self.put_notice()
        outside = self.crate.parent / "external"
        outside.mkdir()
        external_license = outside / "LICENSE"
        external_license.write_bytes(MIT)
        for target in (external_license, outside, inside):
            for absolute in (False, True):
                if target == inside and not absolute:
                    continue
                with self.subTest(directory=target.is_dir(), absolute=absolute):
                    link = self.crate / ("vendor" if target.is_dir() else "NOTICE")
                    destination = str(target.resolve()) if absolute else os.path.relpath(target, self.crate)
                    link.symlink_to(destination, target_is_directory=target.is_dir())
                    try:
                        with self.assertRaises(notices.NoticeError):
                            self.collect()
                    finally:
                        link.unlink()

    def test_internal_relative_symlink_preserves_original_notice_bytes(self):
        data = MIT.replace(b"\n", b"\r\n")
        self.put_notice("legal/terms.txt", data)
        (self.crate / "LICENSE").symlink_to("legal/terms.txt")
        self.package["license_file"] = "LICENSE"
        output = self.base / "bundle"
        notices.write_bundle(output, self.collect(), TARGET)
        self.assertEqual((output / "fixture-1.0.0/LICENSE").read_bytes(), data)

    def test_spdx_and_url_only_files_cannot_substitute_for_license_text(self):
        for data in (
            b"MIT\n", b"SPDX-License-Identifier: MIT\n",
            b"https://example.invalid/LICENSE\n",
            b"MIT License; see https://example.invalid/LICENSE for terms.\n",
        ):
            for explicit in (None, "LICENSE"):
                with self.subTest(data=data, license_file=explicit):
                    self.put_notice(data=data)
                    self.package["license_file"] = explicit
                    with self.assertRaises(notices.NoticeError):
                        self.collect()

    def test_explicit_license_file_cannot_copy_unrelated_metadata(self):
        self.put_notice()
        self.put_notice("settings.toml", b'owner = "SYNTHETIC_OWNER_METADATA"\n')
        self.package["license_file"] = "settings.toml"
        with self.assertRaises(notices.NoticeError):
            self.collect()

    def test_existing_nonempty_output_is_preserved(self):
        self.put_notice()
        output = self.base / "bundle"
        (output / "nested").mkdir(parents=True)
        (output / "INDEX.md").write_bytes(b"Existing release index\n")
        (output / "nested/NOTICE").write_bytes(b"Existing attribution\n")
        before = self.snapshot(output)
        with self.assertRaises(notices.NoticeError):
            notices.write_bundle(output, self.collect(), TARGET)
        self.assertEqual(self.snapshot(output), before)

    def test_output_is_deterministic_and_excludes_local_metadata(self):
        self.put_notice()
        attribution = b"Copyright (c) 2026 Synthetic Codec Contributors\n"
        self.put_notice("vendor/codec/NOTICE", attribution)
        second = self.make_package("another")
        (Path(second["manifest_path"]).parent / "LICENSE").write_bytes(MIT)
        bundles = []
        for number, packages in enumerate(
            ([self.package, second], [second, self.package])
        ):
            output = self.base / ("bundle-" + str(number))
            notices.write_bundle(output, self.collect(packages), TARGET)
            bundles.append(self.snapshot(output))
        self.assertEqual(bundles[0], bundles[1])
        self.assertEqual(
            {name: data for name, data in bundles[0].items() if name != "INDEX.md"},
            {"fixture-1.0.0/LICENSE": MIT, "another-1.0.0/LICENSE": MIT,
             "fixture-1.0.0/vendor/codec/NOTICE": attribution},
        )
        for data in bundles[0].values():
            for forbidden in (
                str(self.base).encode(), str(self.base.resolve()).encode(),
                b"SYNTHETIC_OWNER_METADATA", b"SYNTHETIC_METADATA_DO_NOT_PUBLISH",
                b"manifest_path", b"path+file://",
            ):
                self.assertNotIn(forbidden, data)
        index = bundles[0]["INDEX.md"].decode("utf-8")
        for name in bundles[0]:
            if name != "INDEX.md":
                self.assertIn("](" + name + ")", index)

    def test_conjunction_cannot_package_only_one_required_license(self):
        self.put_notice()
        for expression in (
            "MIT AND Apache-2.0", "(MIT OR Apache-2.0) AND Unicode-3.0",
            "MIT AND (Apache-2.0 OR Unicode-3.0)",
        ):
            with self.subTest(expression=expression):
                self.package["license"] = expression
                with self.assertRaises(notices.NoticeError):
                    self.collect()

    def test_spdx_operators_preserve_required_coverage_and_precedence(self):
        cases = (
            ("MIT OR Apache-2.0", {"MIT"}, True),
            ("MIT/Apache-2.0", {"Apache-2.0"}, True),
            ("MIT AND Apache-2.0", {"MIT"}, False),
            ("MIT AND Apache-2.0", {"MIT", "Apache-2.0"}, True),
            ("(MIT OR Apache-2.0) AND Unicode-3.0", {"MIT"}, False),
            ("(MIT OR Apache-2.0) AND Unicode-3.0", {"Apache-2.0", "Unicode-3.0"}, True),
            ("MIT OR Apache-2.0 AND Unicode-3.0", {"MIT"}, True),
            ("MIT OR Apache-2.0 AND Unicode-3.0", {"Apache-2.0"}, False),
            ("MIT OR Apache-2.0 AND Unicode-3.0", {"Apache-2.0", "Unicode-3.0"}, True),
        )
        for expression, available, expected in cases:
            with self.subTest(expression=expression, available=available):
                self.assertEqual(notices.satisfies(expression, available), expected)

    def test_unsupported_or_incomplete_expressions_fail_even_after_a_match(self):
        for expression in (
            "MIT OR", "MIT AND", "MIT OR (Apache-2.0", "MIT OR Apache-2.0)",
            "MIT WITH Unsupported-exception", "MIT OR Apache-2.0 WITH Unsupported-exception",
        ):
            with self.subTest(expression=expression):
                with self.assertRaises(notices.NoticeError):
                    notices.satisfies(expression, {"MIT"})


if __name__ == "__main__":
    unittest.main()
