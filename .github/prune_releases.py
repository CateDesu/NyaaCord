#!/usr/bin/env python3
"""Prune superseded Nyaacord GitHub releases after verifying the newest release."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Callable

DEFAULT_REPO = "CateDesu/NyaaCord"
APPIMAGE_ASSET = "Nyaacord-x86_64.AppImage"
LEGACY_APPIMAGE_ASSET = "NyaaCord-x86_64.AppImage"
CHECKSUMS_ASSET = "SHA256SUMS.txt"
REQUIRED_ASSETS = (APPIMAGE_ASSET, CHECKSUMS_ASSET)
RELEASE_PROJECTION = (
    ".[] | {id, tag_name, draft, prerelease, "
    "assets: [.assets[] | {id, name, size, state, digest, updated_at}]}"
)

TAG_PATTERN = re.compile(r"^v(\d+)\.(\d+)\.(\d+)(?:\.(\d+))?$")
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")
REPO_PATTERN = re.compile(r"^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$")


def validate_repo(repo: str) -> str:
    if not isinstance(repo, str) or not REPO_PATTERN.fullmatch(repo):
        raise ValueError(f"Invalid repository identifier: {repo!r}")
    owner, name = repo.split("/", 1)
    if owner in {".", ".."} or name in {".", ".."}:
        raise ValueError(f"Invalid repository identifier: {repo!r}")
    return repo


def parse_version(tag: str) -> tuple[int, int, int, int]:
    if not isinstance(tag, str):
        raise ValueError("Use a stable numeric v-prefixed tag")
    match = TAG_PATTERN.fullmatch(tag)
    if match is None:
        raise ValueError("Use a stable numeric v-prefixed tag")
    major, minor, patch, revision = match.groups()
    return (int(major), int(minor), int(patch), int(revision or 0))


def is_numeric_tag(tag: object) -> bool:
    return isinstance(tag, str) and TAG_PATTERN.fullmatch(tag) is not None


def compare_versions(left: str, right: str) -> int:
    a = parse_version(left)
    b = parse_version(right)
    return (a > b) - (a < b)


def is_stable(release: dict) -> bool:
    return release.get("draft") is False and release.get("prerelease") is False


def is_positive_int(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def has_required_assets(release: dict, *, allow_legacy: bool = False) -> bool:
    assets = release.get("assets")
    if not isinstance(assets, list):
        return False
    appimage_names = (APPIMAGE_ASSET, LEGACY_APPIMAGE_ASSET) if allow_legacy else (APPIMAGE_ASSET,)
    has_appimage = any(
        isinstance(asset, dict)
        and asset.get("name") in appimage_names
        and asset.get("state") == "uploaded"
        and is_positive_int(asset.get("size"))
        for asset in assets
    )
    has_sums = any(
        isinstance(asset, dict)
        and asset.get("name") == CHECKSUMS_ASSET
        and asset.get("state") == "uploaded"
        and is_positive_int(asset.get("size"))
        for asset in assets
    )
    return has_appimage and has_sums


def parse_release_stream(text: str) -> list[dict]:
    decoder = json.JSONDecoder()
    releases: list[dict] = []
    idx = 0
    length = len(text)
    while idx < length:
        while idx < length and text[idx].isspace():
            idx += 1
        if idx >= length:
            break
        obj, next_idx = decoder.raw_decode(text, idx)
        if not isinstance(obj, dict):
            raise ValueError("Expected JSON object in release stream")
        releases.append(obj)
        idx = next_idx
    return releases


def parse_sha256sums(text: str, asset_name: str) -> str:
    matched: str | None = None
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split(None, 1)
        if len(parts) != 2:
            continue
        digest, name = parts
        normalized_name = name.lstrip("*").removeprefix("./")
        if normalized_name != asset_name:
            continue
        if SHA256_PATTERN.fullmatch(digest) is None:
            raise ValueError(f"Invalid checksum digest for {asset_name}")
        if matched is not None and matched != digest:
            raise ValueError(f"Conflicting checksum entries for {asset_name}")
        matched = digest
    if matched is None:
        raise ValueError(f"Checksum missing for {asset_name}")
    return matched


def sha256_file(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as fh:
        while True:
            chunk = fh.read(65536)
            if not chunk:
                break
            hasher.update(chunk)
    return hasher.hexdigest()


def is_valid_appimage_header(path: Path) -> bool:
    with path.open("rb") as fh:
        head = fh.read(11)
    return len(head) == 11 and head[:4] == b"\x7fELF" and head[8:11] == b"AI\x02"


def retention_plan(releases: list[dict], tag: str | None = None) -> dict:
    if tag:
        parse_version(tag)
        matches = [r for r in releases if r.get("tag_name") == tag]
        if len(matches) != 1:
            raise ValueError("Published Nyaacord release with required assets is required")
        keep = matches[0]
        if not is_stable(keep) or not has_required_assets(keep):
            raise ValueError("Published Nyaacord release with required assets is required")
        keep_tag = tag
    else:
        candidates = [
            r
            for r in releases
            if is_stable(r) and is_numeric_tag(r.get("tag_name")) and has_required_assets(r)
        ]
        if not candidates:
            raise ValueError("Published Nyaacord release with required assets is required")
        keep = max(candidates, key=lambda r: parse_version(r["tag_name"]))
        keep_tag = keep["tag_name"]

    if any(
        is_stable(r)
        and is_numeric_tag(r.get("tag_name"))
        and compare_versions(r["tag_name"], keep_tag) > 0
        for r in releases
    ):
        raise ValueError("Only the newest stable release may prune older releases")

    remove = [
        r
        for r in releases
        if is_stable(r)
        and is_numeric_tag(r.get("tag_name"))
        and compare_versions(r["tag_name"], keep_tag) < 0
        and has_required_assets(r, allow_legacy=True)
    ]

    if any(not is_positive_int(r.get("id")) for r in [keep, *remove]):
        raise ValueError("Release identity is missing")

    remove.sort(key=lambda r: (parse_version(r["tag_name"]), r["id"]))
    return {"keep": keep, "remove": remove}


def release_identity(release: dict) -> str:
    assets = release.get("assets") or []
    canonical_assets = [
        [asset.get(field) for field in ("id", "name", "size", "state", "digest", "updated_at")]
        for asset in assets
    ]
    return json.dumps(
        [
            release.get("id"),
            release.get("tag_name"),
            release.get("draft"),
            release.get("prerelease"),
            canonical_assets,
        ],
        separators=(",", ":"),
    )


def gh(args: list[str]) -> str:
    result = subprocess.run(["gh", *args], check=False, capture_output=True, text=True)
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or f"gh exited with {result.returncode}"
        raise RuntimeError(detail)
    return result.stdout.strip()


def _single_uploaded_asset(keep: dict, name: str) -> dict:
    matches = [
        asset
        for asset in keep.get("assets", [])
        if isinstance(asset, dict) and asset.get("name") == name
    ]
    if len(matches) != 1:
        raise ValueError(f"Expected single {name} asset on release {keep.get('tag_name')}")
    asset = matches[0]
    if asset.get("state") != "uploaded" or not is_positive_int(asset.get("size")):
        raise ValueError(f"Release asset {name} is incomplete")
    return asset


def verify_published_release(
    repo: str,
    keep: dict,
    local_dir: Path | None,
    verify_appimage_bytes: bool,
    execute: Callable[[list[str]], str],
) -> None:
    sums_asset = _single_uploaded_asset(keep, CHECKSUMS_ASSET)
    app_asset = _single_uploaded_asset(keep, APPIMAGE_ASSET)
    patterns = [CHECKSUMS_ASSET, APPIMAGE_ASSET] if verify_appimage_bytes else [CHECKSUMS_ASSET]

    with tempfile.TemporaryDirectory(prefix="nyaacord-release-retention-") as tmp_dir:
        published = Path(tmp_dir) / "published"
        published.mkdir()
        download_args = [
            "release",
            "download",
            keep["tag_name"],
            "--repo",
            repo,
            "--dir",
            str(published),
        ]
        for pattern in patterns:
            download_args.extend(["--pattern", pattern])
        execute(download_args)

        sums_path = published / CHECKSUMS_ASSET
        if not sums_path.is_file() or sums_path.stat().st_size == 0:
            raise ValueError(f"Missing downloaded {CHECKSUMS_ASSET}")
        if sums_path.stat().st_size != sums_asset["size"]:
            raise ValueError(f"Published asset size mismatch for {CHECKSUMS_ASSET}")

        expected_sha = parse_sha256sums(sums_path.read_text(encoding="utf-8"), APPIMAGE_ASSET)
        sums_digest = sums_asset.get("digest")
        if sums_digest and sums_digest != f"sha256:{sha256_file(sums_path)}":
            raise ValueError(f"Published digest does not match {CHECKSUMS_ASSET}")

        app_digest = app_asset.get("digest")
        if app_digest and app_digest != f"sha256:{expected_sha}":
            raise ValueError(f"Published digest does not match {APPIMAGE_ASSET}")

        if verify_appimage_bytes:
            app_path = published / APPIMAGE_ASSET
            if not app_path.is_file() or app_path.stat().st_size == 0:
                raise ValueError(f"Missing downloaded {APPIMAGE_ASSET}")
            if app_path.stat().st_size != app_asset["size"]:
                raise ValueError(f"Published asset size mismatch for {APPIMAGE_ASSET}")
            if not is_valid_appimage_header(app_path):
                raise ValueError(f"Published {APPIMAGE_ASSET} has an invalid AppImage header")
            if sha256_file(app_path) != expected_sha:
                raise ValueError(f"Published {APPIMAGE_ASSET} checksum mismatch")

        if local_dir is not None:
            local_sums = local_dir / CHECKSUMS_ASSET
            local_app = local_dir / APPIMAGE_ASSET
            if not local_sums.is_file() or not local_app.is_file():
                raise ValueError(f"Local verified assets missing in {local_dir}")
            if local_sums.read_bytes() != sums_path.read_bytes():
                raise ValueError(f"Published asset differs from local build: {CHECKSUMS_ASSET}")
            if not verify_appimage_bytes:
                raise ValueError("Cannot compare local AppImage without downloading published bytes")
            app_path = published / APPIMAGE_ASSET
            if local_app.stat().st_size != app_path.stat().st_size or sha256_file(local_app) != sha256_file(app_path):
                raise ValueError(f"Published asset differs from local build: {APPIMAGE_ASSET}")


def prune_releases(
    tag: str | None = None,
    local_dir: str | Path | None = None,
    *,
    repo: str | None = None,
    dry_run: bool = False,
    execute: Callable[[list[str]], str] = gh,
) -> dict:
    target_repo = validate_repo(repo or os.environ.get("GITHUB_REPOSITORY") or DEFAULT_REPO)
    local_path = Path(local_dir).resolve() if local_dir is not None else None

    def list_releases() -> list[dict]:
        raw = execute(
            [
                "api",
                "--paginate",
                "--jq",
                RELEASE_PROJECTION,
                f"repos/{target_repo}/releases?per_page=100",
            ]
        )
        return parse_release_stream(raw)

    initial = retention_plan(list_releases(), tag)
    keep_tag = initial["keep"]["tag_name"]
    verify_bytes = bool(initial["remove"] or local_path is not None)
    verify_published_release(target_repo, initial["keep"], local_path, verify_bytes, execute)

    if dry_run:
        return {
            "kept": keep_tag,
            "wouldRemove": [release["tag_name"] for release in initial["remove"]],
            "removed": [],
            "tagsRetained": True,
        }

    removed: list[str] = []
    for candidate in initial["remove"]:
        current = retention_plan(list_releases(), keep_tag)
        target = next((r for r in current["remove"] if r["id"] == candidate["id"]), None)
        if (
            release_identity(current["keep"]) != release_identity(initial["keep"])
            or target is None
            or release_identity(target) != release_identity(candidate)
        ):
            raise RuntimeError("Release assets changed during cleanup; nothing further was deleted")

        execute(["api", "--method", "DELETE", f"repos/{target_repo}/releases/{target['id']}"])
        if any(r.get("id") == target["id"] for r in list_releases()):
            raise RuntimeError(f"Release deletion did not land: {target['tag_name']}")
        removed.append(target["tag_name"])
        print(f"Removed superseded release {target['tag_name']}; tag retained", file=sys.stderr)

    return {"kept": keep_tag, "removed": removed, "tagsRetained": True}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Prune superseded Nyaacord GitHub releases.")
    parser.add_argument("--tag", default=None, help="Published tag to keep; defaults to newest stable release")
    parser.add_argument("--dir", dest="local_dir", default=None, help="Optional local dist directory to compare against")
    parser.add_argument("--repo", default=None, help="GitHub owner/name override")
    parser.add_argument("--dry-run", action="store_true", help="Plan and verify without deleting releases")
    args = parser.parse_args(argv)

    try:
        result = prune_releases(
            args.tag or None,
            args.local_dir,
            repo=args.repo or None,
            dry_run=args.dry_run,
        )
    except Exception as exc:  # noqa: BLE001
        print(str(exc), file=sys.stderr)
        return 1

    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
