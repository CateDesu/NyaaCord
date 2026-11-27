#!/usr/bin/env python3
"""Tests for .github/prune_releases.py."""

from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("prune_releases.py")
SPEC = importlib.util.spec_from_file_location("prune_releases", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
prune_mod = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(prune_mod)


def make_appimage_bytes(tag: str) -> bytes:
    return b"\x7fELF\x02\x01\x01\x00AI\x02" + f"nyaacord-{tag}".encode("utf-8")


def make_release(
    release_id: int,
    tag: str,
    *,
    draft: bool = False,
    prerelease: bool = False,
    include_assets: bool = True,
    app_bytes: bytes | None = None,
    appimage_asset: str | None = None,
) -> dict:
    asset_name = appimage_asset or prune_mod.APPIMAGE_ASSET
    payload = app_bytes if app_bytes is not None else make_appimage_bytes(tag)
    app_sha = hashlib.sha256(payload).hexdigest()
    sums_bytes = f"{app_sha}  {asset_name}\n".encode("utf-8")
    sums_sha = hashlib.sha256(sums_bytes).hexdigest()
    assets = []
    if include_assets:
        assets = [
            {
                "id": release_id * 100 + 1,
                "name": asset_name,
                "size": len(payload),
                "state": "uploaded",
                "digest": f"sha256:{app_sha}",
                "updated_at": "2026-11-26T18:00:00Z",
            },
            {
                "id": release_id * 100 + 2,
                "name": prune_mod.CHECKSUMS_ASSET,
                "size": len(sums_bytes),
                "state": "uploaded",
                "digest": f"sha256:{sums_sha}",
                "updated_at": "2026-11-26T18:00:00Z",
            },
        ]
    return {
        "id": release_id,
        "tag_name": tag,
        "draft": draft,
        "prerelease": prerelease,
        "assets": assets,
    }


def sample_releases() -> list[dict]:
    return [
        make_release(1, "v0.0.8", include_assets=False),
        make_release(2, "v0.0.9"),
        make_release(3, "v0.1.0"),
        make_release(4, "v0.2.0"),
    ]


class PruneFixture:
    def __init__(self, fault: str = "", pretty: bool = False) -> None:
        self.fault = fault
        self.pretty = pretty
        self.temp = tempfile.TemporaryDirectory(prefix="nyaacord-prune-test-")
        self.dir = Path(self.temp.name)
        self.rows = sample_releases()
        self.commands: list[list[str]] = []
        self.downloaded = False

        payload = make_appimage_bytes("v0.2.0")
        sums = f"{hashlib.sha256(payload).hexdigest()}  {prune_mod.APPIMAGE_ASSET}\n".encode("utf-8")
        self.files = {
            prune_mod.APPIMAGE_ASSET: payload,
            prune_mod.CHECKSUMS_ASSET: sums,
        }
        (self.dir / prune_mod.APPIMAGE_ASSET).write_bytes(payload)
        (self.dir / prune_mod.CHECKSUMS_ASSET).write_bytes(sums)

    def cleanup(self) -> None:
        self.temp.cleanup()

    def execute(self, args: list[str]) -> str:
        self.commands.append(list(args))
        if "--paginate" in args:
            assert "--jq" in args
            assert "--slurp" not in args
            if self.downloaded and self.fault == "newer":
                self.rows = [*[r for r in self.rows if r["id"] != 9], make_release(9, "v0.3.0")]
            if self.downloaded and self.fault == "changed":
                self.rows[-1]["assets"][0]["id"] += 1
            indent = 2 if self.pretty else None
            return "\n".join(json.dumps(row, indent=indent) for row in self.rows)

        if args[:2] == ["release", "download"]:
            tag = args[2]
            destination = Path(args[args.index("--dir") + 1])
            for index, value in enumerate(args):
                if value != "--pattern":
                    continue
                name = args[index + 1]
                if self.fault == "missing" and name == prune_mod.APPIMAGE_ASSET:
                    raise RuntimeError("Asset missing")
                content = self.files[name]
                if self.fault == "corrupt_bytes" and tag == "v0.2.0" and name == prune_mod.APPIMAGE_ASSET:
                    content = b"\x7fELF\x02\x01\x01\x00AI\x02" + b"corrupted!"
                elif self.fault == "bad_magic" and tag == "v0.2.0" and name == prune_mod.APPIMAGE_ASSET:
                    content = b"NOTANAPPIMAGE" + b"x" * (len(content) - 13)
                elif self.fault == "uppercase_sums" and name == prune_mod.CHECKSUMS_ASSET:
                    digest, rest = content.decode("utf-8").split("  ", 1)
                    content = f"{digest.upper()}  {rest}".encode("utf-8")
                elif self.fault == "mismatch_sums" and name == prune_mod.CHECKSUMS_ASSET:
                    content = f"{'0' * 64}  {prune_mod.APPIMAGE_ASSET}\n".encode("utf-8")
                (destination / name).write_bytes(content)
            self.downloaded = True
            return ""

        assert args[:3] == ["api", "--method", "DELETE"]
        release_id = int(args[3].rsplit("/", 1)[-1])
        if self.fault != "not_deleted":
            self.rows = [row for row in self.rows if row["id"] != release_id]
        return ""


class PruneReleasesTests(unittest.TestCase):
    def test_keeps_newest_drafts_prereleases_and_non_numeric_tags(self) -> None:
        rows = sample_releases()
        rows.extend(
            [
                make_release(5, "v0.1.5", draft=True),
                make_release(6, "v0.3.0", prerelease=True),
                make_release(7, "historical-build", include_assets=True),
                make_release(8, "v0.1.1"),
            ]
        )
        rows[2]["assets"].append(
            {
                "id": 999,
                "name": "legacy-note.txt",
                "size": 12,
                "state": "uploaded",
                "digest": None,
                "updated_at": "2026-11-26T18:00:00Z",
            }
        )
        plan = prune_mod.retention_plan(rows, "v0.2.0")
        self.assertEqual(plan["keep"]["tag_name"], "v0.2.0")
        self.assertEqual([r["tag_name"] for r in plan["remove"]], ["v0.0.9", "v0.1.0", "v0.1.1"])

    def test_auto_detects_newest_tag_and_rejects_unreleased_newer_version(self) -> None:
        rows = sample_releases()
        auto_plan = prune_mod.retention_plan(rows)
        self.assertEqual(auto_plan["keep"]["tag_name"], "v0.2.0")
        self.assertEqual([r["tag_name"] for r in auto_plan["remove"]], ["v0.0.9", "v0.1.0"])

        rows.append(make_release(9, "v0.3.0", include_assets=False))
        with self.assertRaisesRegex(ValueError, "newest stable release"):
            prune_mod.retention_plan(rows)

    def test_refuses_stale_missing_draft_prerelease_or_incomplete_keep(self) -> None:
        with self.assertRaisesRegex(ValueError, "newest"):
            prune_mod.retention_plan(sample_releases(), "v0.1.0")
        with self.assertRaisesRegex(ValueError, "required"):
            prune_mod.retention_plan(sample_releases()[:-1], "v0.2.0")
        for field in ("draft", "prerelease"):
            rows = sample_releases()
            rows[-1][field] = True
            with self.assertRaisesRegex(ValueError, "required"):
                prune_mod.retention_plan(rows, "v0.2.0")
        rows = sample_releases()
        rows[-1]["assets"].pop()
        with self.assertRaisesRegex(ValueError, "required"):
            prune_mod.retention_plan(rows, "v0.2.0")
        rows = sample_releases()
        rows[-1]["assets"][0]["state"] = "starter"
        with self.assertRaisesRegex(ValueError, "required"):
            prune_mod.retention_plan(rows, "v0.2.0")

    def test_preserves_incomplete_older_releases_and_checks_ids(self) -> None:
        rows = sample_releases()
        rows[1]["assets"].pop()
        plan = prune_mod.retention_plan(rows, "v0.2.0")
        self.assertEqual([r["id"] for r in plan["remove"]], [3])

        rows = sample_releases()
        rows[-1]["id"] = True
        with self.assertRaisesRegex(ValueError, "identity"):
            prune_mod.retention_plan(rows, "v0.2.0")

    def test_verifies_published_bytes_before_deleting_releases_only(self) -> None:
        fixture = PruneFixture(pretty=True)
        self.addCleanup(fixture.cleanup)
        result = prune_mod.prune_releases("v0.2.0", fixture.dir, execute=fixture.execute)
        self.assertEqual(result, {"kept": "v0.2.0", "removed": ["v0.0.9", "v0.1.0"], "tagsRetained": True})
        self.assertEqual([r["tag_name"] for r in fixture.rows], ["v0.0.8", "v0.2.0"])
        deletes = [i for i, cmd in enumerate(fixture.commands) if "DELETE" in cmd]
        downloads = [i for i, cmd in enumerate(fixture.commands) if cmd[:2] == ["release", "download"]]
        self.assertEqual(len(deletes), 2)
        self.assertTrue(min(deletes) > max(downloads))
        self.assertFalse(any("git/refs" in arg or "--cleanup-tag" in arg for cmd in fixture.commands for arg in cmd))

    def test_faults_prevent_all_deletions(self) -> None:
        for fault in ("corrupt_bytes", "bad_magic", "uppercase_sums", "mismatch_sums", "missing", "newer", "changed"):
            with self.subTest(fault=fault):
                fixture = PruneFixture(fault=fault)
                self.addCleanup(fixture.cleanup)
                with self.assertRaises(Exception):
                    prune_mod.prune_releases("v0.2.0", fixture.dir, execute=fixture.execute)
                self.assertEqual(sum("DELETE" in cmd for cmd in fixture.commands), 0)

    def test_stops_if_deletion_does_not_land(self) -> None:
        fixture = PruneFixture(fault="not_deleted")
        self.addCleanup(fixture.cleanup)
        with self.assertRaisesRegex(RuntimeError, "did not land"):
            prune_mod.prune_releases("v0.2.0", fixture.dir, execute=fixture.execute)
        self.assertEqual(sum("DELETE" in cmd for cmd in fixture.commands), 1)

    def test_honors_github_repository_and_dry_run(self) -> None:
        fixture = PruneFixture()
        self.addCleanup(fixture.cleanup)
        prev = os.environ.get("GITHUB_REPOSITORY")
        os.environ["GITHUB_REPOSITORY"] = "ForkOwner/Nyaacord"
        try:
            result = prune_mod.prune_releases(dry_run=True, local_dir=fixture.dir, execute=fixture.execute)
        finally:
            if prev is None:
                os.environ.pop("GITHUB_REPOSITORY", None)
            else:
                os.environ["GITHUB_REPOSITORY"] = prev
        self.assertEqual(
            result,
            {"kept": "v0.2.0", "wouldRemove": ["v0.0.9", "v0.1.0"], "removed": [], "tagsRetained": True},
        )
        self.assertIn("repos/ForkOwner/Nyaacord/releases?per_page=100", fixture.commands[0])
        self.assertEqual(sum("DELETE" in cmd for cmd in fixture.commands), 0)

    def test_prunes_legacy_v021_asset_when_v022_is_newest(self) -> None:
        rows = [
            make_release(10, "v0.2.1", appimage_asset=prune_mod.LEGACY_APPIMAGE_ASSET),
            make_release(11, "v0.2.2", appimage_asset=prune_mod.APPIMAGE_ASSET),
        ]
        plan = prune_mod.retention_plan(rows)
        self.assertEqual(plan["keep"]["tag_name"], "v0.2.2")
        self.assertEqual([r["tag_name"] for r in plan["remove"]], ["v0.2.1"])

        legacy_only = [
            make_release(10, "v0.2.1", appimage_asset=prune_mod.LEGACY_APPIMAGE_ASSET),
        ]
        with self.assertRaisesRegex(ValueError, "required"):
            prune_mod.retention_plan(legacy_only)

    def test_cli_handles_large_paginated_pretty_and_compact_stream(self) -> None:
        fixture = PruneFixture()
        self.addCleanup(fixture.cleanup)
        bin_dir = fixture.dir / "bin"
        bin_dir.mkdir()
        releases_file = fixture.dir / "large-releases.json"
        padding = "x" * 3500
        drafts = [
            {"id": 1000 + i, "tag_name": f"v0.0.1.{i}-{padding}", "draft": True, "prerelease": False, "assets": []}
            for i in range(450)
        ]
        all_rows = [*fixture.rows, *drafts]
        self.assertGreater(len(json.dumps(all_rows)), 1_500_000)
        releases_file.write_text(json.dumps(all_rows), encoding="utf-8")

        fake_gh = bin_dir / "gh"
        fake_gh.write_text(
            """#!/usr/bin/env python3
import json, os, shutil, sys
from pathlib import Path
args = sys.argv[1:]
db = Path(os.environ["NYAACORD_TEST_RELEASES"])
if args[:1] == ["api"] and "--paginate" in args:
    assert "--jq" in args and "--slurp" not in args
    rows = json.loads(db.read_text(encoding="utf-8"))
    for idx, row in enumerate(rows):
        print(json.dumps(row, indent=2 if idx % 2 == 0 else None))
elif args[:2] == ["release", "download"]:
    dest = Path(args[args.index("--dir") + 1])
    src = Path(os.environ["NYAACORD_TEST_ASSETS"])
    for idx, val in enumerate(args):
        if val == "--pattern":
            name = args[idx + 1]
            shutil.copyfile(src / name, dest / name)
elif args[:3] == ["api", "--method", "DELETE"]:
    target_id = int(args[-1].rsplit("/", 1)[-1])
    rows = [r for r in json.loads(db.read_text(encoding="utf-8")) if r["id"] != target_id]
    db.write_text(json.dumps(rows), encoding="utf-8")
else:
    sys.exit(2)
""",
            encoding="utf-8",
        )
        fake_gh.chmod(0o755)

        env = os.environ.copy()
        env["PATH"] = f"{bin_dir}:{env.get('PATH', '')}"
        env["NYAACORD_TEST_RELEASES"] = str(releases_file)
        env["NYAACORD_TEST_ASSETS"] = str(fixture.dir)
        proc = subprocess.run(
            [sys.executable, str(MODULE_PATH), "--dir", str(fixture.dir)],
            capture_output=True,
            text=True,
            env=env,
            check=False,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        output = json.loads(proc.stdout.strip().splitlines()[-1])
        self.assertEqual(output, {"kept": "v0.2.0", "removed": ["v0.0.9", "v0.1.0"], "tagsRetained": True})


if __name__ == "__main__":
    unittest.main()
