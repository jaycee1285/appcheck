#!/usr/bin/env python3
"""Inventory install possibilities without changing the ledger or installing files."""

import argparse
import base64
import concurrent.futures
import json
import re
import subprocess
import tarfile
import tempfile
import tomllib
import urllib.error
import urllib.request
import zipfile
from pathlib import Path


def gh(path):
    result = subprocess.run(["gh", "api", path], capture_output=True, text=True, timeout=30)
    if result.returncode:
        return None
    return json.loads(result.stdout)


def crate(package):
    request = urllib.request.Request(
        f"https://crates.io/api/v1/crates/{package}",
        headers={"User-Agent": "apptrack-source-probe/0.1"},
    )
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            item = json.load(response)["crate"]
        return {key: item.get(key) for key in ("name", "max_stable_version", "repository", "description")}
    except (urllib.error.URLError, TimeoutError, KeyError, ValueError):
        return None


def cargo_source(repo, source, location, manifest):
    package = manifest["package"]
    registry = crate(package["name"])
    if not registry:
        return None
    info = subprocess.run(
        ["cargo", "info", package["name"], "--registry", "crates-io", "--color", "never"],
        capture_output=True, text=True, timeout=30,
    )
    selected = re.search(r"^version: ([^\s]+)", info.stdout, re.MULTILINE) if info.returncode == 0 else None
    return {
        "package": package["name"],
        "bins": [item["name"] for item in manifest.get("bin", []) if "name" in item],
        "main_rs": gh(f"repos/{repo}/contents/{location}src/main.rs") is not None,
        "location": location,
        "registry": registry,
        "selected_version": selected.group(1) if selected else None,
        "repository_matches": (registry.get("repository") or "").lower().removesuffix(".git").rstrip("/") == source.lower().removesuffix(".git").rstrip("/"),
    }


def linux_asset(name):
    lower = name.lower()
    if lower.endswith((".deb", ".rpm", ".whl", ".apk", ".msi", ".pkg", ".sha", ".sha256", ".sha512", ".sig", ".asc", ".txt", ".json", ".yml")):
        return False
    if any(token in lower for token in ("windows", "darwin", "macos", "apple", ".app.tar")):
        return False
    if lower.endswith(".appimage") and not any(token in lower for token in ("aarch64", "arm64", "armv7", "i386", "i686")):
        return True  # Unqualified AppImages still need an x86_64 header check.
    arch = any(token in lower for token in ("x86_64", "amd64", "x64"))
    return (arch and ("linux" in lower or lower.endswith(".appimage"))) or "linux64" in lower or (lower.endswith((".tar.gz", ".tar.xz")) and any(token in lower for token in ("x86_64", "amd64")))


def inspect(app):
    source = app.get("identity", "")
    record = {"name": app["name"], "identity": source, "disposition": app["disposition"]}
    if not source.startswith("https://github.com/"):
        record["status"] = "source requires a different forge or manual review"
        return record
    repo = source.removeprefix("https://github.com/").strip("/")
    release = gh(f"repos/{repo}/releases/latest")
    if release is None:
        record["release"] = "no stable GitHub release"
    else:
        record["release"] = release["tag_name"]
        record["all_assets"] = [{"name": item["name"], "size": item["size"]} for item in release.get("assets", [])]
        record["linux_x86_64_assets"] = [
            {"name": item["name"], "size": item["size"], "api_url": item["url"]}
            for item in release.get("assets", [])
            if linux_asset(item["name"])
        ]
    manifest = gh(f"repos/{repo}/contents/Cargo.toml")
    if manifest and manifest.get("encoding") == "base64":
        try:
            cargo = tomllib.loads(base64.b64decode(manifest["content"]).decode())
            package = cargo.get("package", {})
            if package.get("name"):
                record["cargo_manifest"] = {
                    "package": package["name"],
                    "bins": [item["name"] for item in cargo.get("bin", []) if "name" in item],
                    "default_bin": package.get("default-run"),
                }
                record["cargo_manifest"]["main_rs"] = gh(f"repos/{repo}/contents/src/main.rs") is not None
                reviewed = cargo_source(repo, source, "", cargo)
                if reviewed:
                    record["cargo_registry"] = reviewed["registry"]
                    record["cargo_selected_version"] = reviewed["selected_version"]
                    record["cargo_repository_matches"] = (
                        reviewed["repository_matches"]
                    )
            members = cargo.get("workspace", {}).get("members", [])
            if len(members) <= 16:
                workspace = []
                for member in members:
                    if not isinstance(member, str) or not all(re.fullmatch(r"[A-Za-z0-9._-]+", part) for part in member.split("/")):
                        continue
                    response = gh(f"repos/{repo}/contents/{member}/Cargo.toml")
                    if not response or response.get("encoding") != "base64":
                        continue
                    data = tomllib.loads(base64.b64decode(response["content"]).decode())
                    if data.get("package", {}).get("name"):
                        reviewed = cargo_source(repo, source, f"{member}/", data)
                        if reviewed:
                            workspace.append(reviewed)
                if workspace:
                    record["cargo_workspace"] = workspace
        except (ValueError, KeyError, UnicodeError):
            record["cargo_manifest"] = "unreadable"
    return record


def elf_x86_64(header):
    return len(header) >= 20 and header[:4] == b"\x7fELF" and header[4] == 2 and int.from_bytes(header[18:20], "little") == 62


def inspect_asset(record, limit):
    assets = record.get("linux_x86_64_assets", [])
    if len(assets) != 1:
        return record
    asset = assets[0]
    if asset["size"] > limit:
        record["artifact_review"] = "over inspection size limit"
        return record
    name = asset["name"]
    repo = record["identity"].removeprefix("https://github.com/")
    with tempfile.TemporaryDirectory(prefix="apptrack-asset-", dir="/tmp") as staging:
        result = subprocess.run(
            ["gh", "release", "download", record["release"], "--repo", repo, "--pattern", name, "--dir", staging],
            capture_output=True, text=True, timeout=180,
        )
        if result.returncode:
            record["artifact_review"] = f"download failed: {result.stderr.strip()[:180]}"
            return record
        path = Path(staging) / name
        members = []
        try:
            if name.endswith((".tar.gz", ".tar.xz")):
                with tarfile.open(path) as archive:
                    for item in archive:
                        if item.isfile() and elf_x86_64(archive.extractfile(item).read(20)):
                            members.append(item.name)
            elif name.endswith(".zip"):
                with zipfile.ZipFile(path) as archive:
                    for item in archive.infolist():
                        if not item.is_dir() and elf_x86_64(archive.open(item).read(20)):
                            members.append(item.filename)
            elif name.lower().endswith(".appimage"):
                record["artifact_review"] = "AppImage needs Type-2 verification"
            elif elf_x86_64(path.open("rb").read(20)):
                members.append(name)
        except (OSError, tarfile.TarError, zipfile.BadZipFile) as error:
            record["artifact_review"] = f"archive inspection failed: {error}"
        record["elf_members"] = members
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ledger", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--inspect", action="store_true", help="Download unique assets under 50 MiB into temporary staging and list x86_64 ELF members")
    args = parser.parse_args()
    apps = tomllib.loads(args.ledger.read_text())["apps"]
    candidates = [app for app in apps if app["disposition"] == "considering" and not app.get("recipe") and app.get("identity", "").startswith("https://github.com/")]
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        results = list(pool.map(inspect, candidates))
    if args.inspect:
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            results = list(pool.map(lambda record: inspect_asset(record, 50 * 1024 * 1024), results))
    args.out.write_text(json.dumps(results, indent=2) + "\n")
    print(f"Probed {len(results)} GitHub sources; report: {args.out}")
    print(f"Stable release: {sum('linux_x86_64_assets' in r for r in results)}; Linux x86_64 assets: {sum(bool(r.get('linux_x86_64_assets')) for r in results)}; root Cargo manifest: {sum(isinstance(r.get('cargo_manifest'), dict) for r in results)}")
    if args.inspect:
        print(f"Single ELF member: {sum(len(r.get('elf_members', [])) == 1 for r in results)}; multi-member: {sum(len(r.get('elf_members', [])) > 1 for r in results)}")


if __name__ == "__main__":
    main()
