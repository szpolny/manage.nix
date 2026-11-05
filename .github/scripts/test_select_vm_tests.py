"""Regression coverage for conservative VM selection and release-only diffs."""

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "selector", Path(__file__).with_name("select-vm-tests.py")
)
selector = importlib.util.module_from_spec(spec)
spec.loader.exec_module(selector)


class SelectionTests(unittest.TestCase):
    def test_docs_skip_vms(self):
        self.assertEqual(selector.select(["README.md", "docs/development.md"]), [])

    def test_web_changes_run_browser(self):
        self.assertEqual(selector.select(["web/src/routes/+page.svelte"]), ["vm-web"])

    def test_unknown_changes_keep_core_coverage(self):
        self.assertEqual(
            set(selector.select(["crates/server/src/main.rs"])), selector.CORE
        )

    def test_shared_fixtures_keep_both_backends(self):
        self.assertEqual(
            set(selector.select(["nix/tests/deploy/revision-a.nix"])),
            {"vm-deploy-sqlite", "vm-deploy-postgres", "vm-web", "vm-fleet"},
        )

    def test_missing_revision_keeps_coverage(self):
        self.assertEqual(set(selector.select(["Cargo.toml"])), selector.CORE)

    def select_contents(self, path, before, after):
        with patch.object(
            selector.subprocess, "check_output", side_effect=[before, after]
        ):
            return selector.select([path], "base", "head")

    def test_release_version_fields_skip_vms(self):
        fixtures = {
            "Cargo.toml": '[workspace.package]\nversion = "OLD"\nedition = "2021"\n',
            "Cargo.lock": 'version = 4\n[[package]]\nname = "mnix-cli"\nversion = "OLD"\n',
            "web/package.json": '{"version":"OLD","dependencies":{"svelte":"5"}}',
            "web/package-lock.json": '{"version":"OLD","packages":{"":{"version":"OLD"},"node_modules/svelte":{"version":"5"}}}',
            ".release-please-manifest.json": '{".":"OLD"}',
            "version.txt": "OLD\n",
        }
        for path, content in fixtures.items():
            with self.subTest(path=path):
                self.assertEqual(
                    self.select_contents(path, content, content.replace("OLD", "NEW")),
                    [],
                )

    def test_cargo_dependency_update_runs_core(self):
        before = 'version = 4\n[[package]]\nname = "serde"\nversion = "1.0.0"\nsource = "registry+https://example.org"\n'
        self.assertEqual(
            set(
                self.select_contents(
                    "Cargo.lock", before, before.replace("1.0.0", "1.0.1")
                )
            ),
            selector.CORE,
        )

    def test_workspace_config_change_runs_core(self):
        before = '[workspace.package]\nversion = "1.0.0"\nedition = "2021"\n'
        self.assertEqual(
            set(
                self.select_contents(
                    "Cargo.toml", before, before.replace("2021", "2024")
                )
            ),
            selector.CORE,
        )

    def test_npm_dependency_update_runs_browser(self):
        for path in ("web/package.json", "web/package-lock.json"):
            before = json.dumps(
                {
                    "version": "1",
                    "dependencies": {"svelte": "5"},
                    "packages": {"": {"version": "1"}},
                }
            )
            with self.subTest(path=path):
                self.assertEqual(
                    self.select_contents(path, before, before.replace('"5"', '"6"')),
                    ["vm-web"],
                )

    def test_invalid_metadata_keeps_coverage(self):
        for path in ("Cargo.toml", "Cargo.lock", ".release-please-manifest.json"):
            with self.subTest(path=path):
                self.assertEqual(
                    set(self.select_contents(path, "invalid", "invalid")), selector.CORE
                )

    def test_missing_file_keeps_coverage(self):
        with patch.object(
            selector.subprocess,
            "check_output",
            side_effect=subprocess.CalledProcessError(128, "git"),
        ):
            self.assertEqual(
                set(selector.select(["Cargo.toml"], "base", "head")), selector.CORE
            )

    def test_release_plus_code_still_runs_core(self):
        with patch.object(
            selector,
            "version_only",
            side_effect=lambda path, *_: path in selector.VERSION_FILES,
        ):
            self.assertEqual(
                set(
                    selector.select(
                        ["Cargo.toml", "web/package.json", "crates/agent/src/main.rs"],
                        "base",
                        "head",
                    )
                ),
                selector.CORE,
            )

    def test_git_diff_and_event_handling(self):
        script = str(Path(selector.__file__).resolve())
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args):
                return subprocess.check_output(
                    ["git", *args], cwd=root, text=True, stderr=subprocess.DEVNULL
                ).strip()

            git("init")
            git("config", "user.name", "CI selector test")
            git("config", "user.email", "test@example.invalid")
            manifest = root / "Cargo.toml"
            manifest.write_text('[workspace.package]\nversion = "1.0.0"\n')
            git("add", ".")
            git("commit", "-m", "test: initial fixture")
            base = git("rev-parse", "HEAD")
            manifest.write_text('[workspace.package]\nversion = "1.0.1"\n')
            git("add", ".")
            git("commit", "-m", "test: release fixture")
            head = git("rev-parse", "HEAD")
            output = root / "output"
            for event, previous, expected in (
                ("push", base, set()),
                ("pull_request", base, set()),
                ("schedule", "", selector.ALL),
                ("workflow_dispatch", "", selector.ALL),
                ("push", "0" * 40, selector.CORE),
                ("push", "missing-revision", selector.CORE),
            ):
                with self.subTest(event=event, previous=previous):
                    output.write_text("")
                    subprocess.run(
                        [sys.executable, script],
                        cwd=root,
                        check=True,
                        env={
                            **os.environ,
                            "EVENT_NAME": event,
                            "BASE_SHA": previous,
                            "HEAD_SHA": head,
                            "GITHUB_OUTPUT": str(output),
                        },
                        capture_output=True,
                        text=True,
                    )
                    values = dict(
                        line.split("=", 1) for line in output.read_text().splitlines()
                    )
                    self.assertEqual(values["has-tests"], str(bool(expected)).lower())
                    self.assertEqual(
                        set(json.loads(values["tests"])), expected or {"vm-hello"}
                    )


if __name__ == "__main__":
    unittest.main()
