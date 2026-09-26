#!/usr/bin/env python3
"""Add reviewed GitHub recipes to the bootstrap ledger from probe_sources.py output."""

import argparse
import json
import os
import re
import tempfile
import tomllib
from pathlib import Path


# Commands that differ from the display name or have an existing imported path.
COMMANDS = {
    "toast": "toast-linux-amd64",
    "yara-code": "ycode",
    "jcode": "jcode",
    "codexia": "codexia-web",
    "cohors": "cohors",
    "omniget": "omniget-cli",
    "clin-rs": "clin",
}


def pattern(name, tag):
    version = tag.removeprefix("v")
    for token, replacement in ((tag, "{tag}"), (version, "{version}")):
        match = re.search(rf"(^|[-_]){re.escape(token)}(?=[-_./])", name)
        if match:
            start = match.end() - len(token)
            end = match.end()
            if not (name[end:end + 1] == "." and name[end + 1:end + 2].isdigit()):
                return name[:start] + replacement + name[end:]
    return name


def quoted(text):
    return json.dumps(text, ensure_ascii=False)


def recipe(app, result):
    assets = result.get("linux_x86_64_assets", [])
    members = result.get("elf_members", [])
    if len(assets) != 1 or len(members) != 1:
        return None
    tags = set(app.get("tags", []))
    if tags.intersection(("tui", "cli")) and "gui" in tags:
        return None
    if not tags.intersection(("tui", "cli", "gui")):
        return None
    asset = assets[0]["name"]
    kind = next((ext.removeprefix(".") for ext in (".tar.gz", ".tar.xz", ".zip") if asset.endswith(ext)), "binary-copy")
    repo = app["identity"].removeprefix("https://github.com/")
    if f"/repos/{repo.lower()}/" not in assets[0].get("api_url", "").lower():
        return None
    command = COMMANDS.get(app["name"], app["name"])
    if app.get("launch"):
        existing = app["launch"]["program"]
        if not existing.startswith("~/.local/bin/"):
            return None
        command = existing.rsplit("/", 1)[-1]
    if not re.fullmatch(r"[A-Za-z0-9._-]+", command):
        return None
    dest = f"~/.local/bin/{command}"
    lines = ["[apps.recipe]", 'source = "github"', f"repo = {quoted(repo)}",
             f"asset = {quoted(pattern(asset, result['release']))}",
             f"installer = {quoted(kind)}", f"destination = {quoted(dest)}",
             'os = "linux"', 'arch = "x86_64"', "clean_filename = false"]
    if kind != "binary-copy":
        lines.append(f"member = {quoted(pattern(members[0].removeprefix('./'), result['release']))}")
    if not app.get("launch"):
        lines.extend(["", "[apps.launch]", f"program = {quoted(dest)}", "args = []",
                      f"gui = {'true' if 'gui' in tags else 'false'}"])
    return "\n".join(lines) + "\n"


def cargo_recipe(app, result):
    manifest = result.get("cargo_manifest")
    registry = result.get("cargo_registry")
    if not manifest or not registry or not result.get("cargo_repository_matches") or not registry.get("max_stable_version"):
        return None
    if result.get("cargo_selected_version") != registry["max_stable_version"]:
        return None
    if any(word in (registry.get("description") or "").lower() for word in ("placeholder", "obsolete")):
        return None
    bins = manifest["bins"] or ([manifest["package"]] if manifest["main_rs"] else [])
    if len(bins) != 1 or not re.fullmatch(r"[A-Za-z0-9._-]+", bins[0]):
        return None
    if not set(app.get("tags", [])).intersection(("tui", "cli")):
        return None
    bin_name = bins[0]
    dest = f"~/.cargo/bin/{bin_name}"
    lines = ["[apps.recipe]", 'source = "cargo"', 'installer = "cargo-install"',
             f"package = {quoted(manifest['package'])}", 'registry = "crates-io"',
             'root = "~/.cargo"', f"bins = [{quoted(bin_name)}]"]
    if not app.get("launch"):
        lines.extend(["", "[apps.launch]", f"program = {quoted(dest)}", "args = []", "gui = false"])
    elif app["launch"]["program"] not in app.get("installed_paths", []):
        return None
    return "\n".join(lines) + "\n"


def appimage_recipe(app, result, headers):
    assets = [a for a in result.get("linux_x86_64_assets", []) if a["name"].lower().endswith(".appimage")]
    if len(assets) != 1 or "gui" not in app.get("tags", []):
        return None
    asset = assets[0]
    reviewed = headers.get(app["name"])
    if not reviewed or reviewed.get("asset") != asset["name"] or not all(
        reviewed.get(key) for key in ("elf", "type2")
    ) or reviewed.get("arch") != 62 or reviewed.get("status") != 206:
        return None
    if not reviewed.get("range", "").startswith("bytes 0-19/"):
        return None
    repo = app["identity"].removeprefix("https://github.com/")
    if f"/repos/{repo.lower()}/" not in asset.get("api_url", "").lower():
        return None
    asset_pattern = pattern(asset["name"], result["release"])
    if re.search(r"\d+\.\d+\.\d+", asset_pattern) and not any(
        token in asset_pattern for token in ("{tag}", "{version}")
    ):
        return None
    command = app["name"]
    if not re.fullmatch(r"[A-Za-z0-9._-]+", command) or app.get("launch"):
        return None
    dest = f"~/.local/bin/{command}"
    lines = ["[apps.recipe]", 'source = "github"', f"repo = {quoted(repo)}",
             f"asset = {quoted(asset_pattern)}", 'installer = "appimage"',
             f"destination = {quoted(dest)}", 'os = "linux"',
             'arch = "x86_64"', "", "[apps.launch]", 'program = "appimage-run"',
             f"args = [{quoted(dest)}]", "gui = true"]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ledger", type=Path)
    parser.add_argument("report", type=Path)
    parser.add_argument("--method", choices=("github", "cargo", "appimage"), default="github")
    parser.add_argument("--headers", type=Path, help="Verified byte-range header report for AppImages")
    parser.add_argument("--only", help="Comma-separated display names reviewed for this pass")
    parser.add_argument("--apply", action="store_true")
    args = parser.parse_args()
    if args.apply and not args.only:
        parser.error("--apply requires an explicit --only list of reviewed applications")
    if args.method == "appimage" and not args.headers:
        parser.error("--method appimage requires --headers")
    original = args.ledger.read_text()
    apps = tomllib.loads(original)["apps"]
    reports = {r["identity"].lower(): r for r in json.loads(args.report.read_text())}
    headers = {r["name"]: r for r in json.loads(args.headers.read_text())} if args.headers else {}
    only = set(args.only.split(",")) if args.only else None
    parts = re.split(r"(?=^\[\[apps\]\]\s*$)", original, flags=re.MULTILINE)
    prefix, blocks = parts[0], parts[1:]
    if len(blocks) != len(apps):
        raise SystemExit("Ledger app blocks do not match parsed records")
    changed = []
    output = [prefix]
    occupied = {path for app in apps for path in app.get("installed_paths", [])}
    occupied.update(app["recipe"]["destination"] for app in apps if app.get("recipe") and app["recipe"].get("destination"))
    for app, block in zip(apps, blocks):
        result = reports.get(app["identity"].lower())
        if args.method == "cargo":
            selected_recipe = cargo_recipe
        elif args.method == "appimage":
            selected_recipe = lambda app, result: appimage_recipe(app, result, headers)
        else:
            selected_recipe = recipe
        addition = selected_recipe(app, result) if result and not app.get("recipe") and app["disposition"] == "considering" and (only is None or app["name"] in only) else None
        if addition:
            candidate = tomllib.loads("schema_version = 1\n[[apps]]\n" + addition)["apps"][0]
            dest = candidate["recipe"].get("destination") or (
                f"{candidate['recipe']['root']}/bin/{candidate['recipe']['bins'][0]}"
                if candidate["recipe"].get("source") == "cargo" else None
            )
            if dest:
                if dest in occupied and dest not in app.get("installed_paths", []):
                    raise SystemExit(f"Destination collision for {app['name']}: {dest}")
                occupied.add(dest)
            # Keep category headings and comments attached to the next record.
            lines = block.splitlines(keepends=True)
            tail = []
            while lines and (not lines[-1].strip() or lines[-1].lstrip().startswith("#")):
                tail.insert(0, lines.pop())
            block = "".join(lines).rstrip("\n") + "\n\n" + addition + "".join(tail)
            changed.append(app["name"])
        output.append(block)
    revised = "".join(output)
    parsed = tomllib.loads(revised)["apps"]
    if len(parsed) != len(apps) or any(a["identity"] != b["identity"] for a, b in zip(apps, parsed)):
        raise SystemExit("Ledger changed identity or record count")
    print(f"Exact recipes: {len(changed)}: {', '.join(changed)}")
    if args.apply:
        with tempfile.NamedTemporaryFile(mode="w", dir=args.ledger.parent, prefix=".apptrack-recipes-", delete=False) as temp:
            temp.write(revised)
            temp.flush()
            os.fsync(temp.fileno())
            name = temp.name
        os.chmod(name, args.ledger.stat().st_mode)
        os.replace(name, args.ledger)
        print(f"Updated {args.ledger}")


if __name__ == "__main__":
    main()
