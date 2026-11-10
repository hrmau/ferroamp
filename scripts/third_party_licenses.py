#!/usr/bin/env python3
"""Writes the license texts of every crate built into the app, for a release.

Only libraries that end up in the binary count: build tools and test-only
crates are left out. Run from the repository root; prints to stdout.
"""
import json
import os
import subprocess
import sys

TARGET = "aarch64-apple-darwin"
NAMES = ("license", "licence", "copying", "notice", "copyright")


def main():
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", TARGET]))
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = meta["resolve"]["root"]

    # Walk normal dependencies only, from this crate outwards.
    shipped, todo = set(), [root]
    while todo:
        node = nodes[todo.pop()]
        for dep in node["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]) and dep["pkg"] not in shipped:
                shipped.add(dep["pkg"])
                todo.append(dep["pkg"])

    out = sys.stdout
    out.write("Third-party software in ferroamp\n")
    out.write("================================\n\n")
    out.write("ferroamp includes the following open-source libraries. Each is used\n")
    out.write("under the license shown, whose full text follows its name.\n")
    for pid in sorted(shipped, key=lambda i: (packages[i]["name"], packages[i]["version"])):
        pkg = packages[pid]
        folder = os.path.dirname(pkg["manifest_path"])
        out.write("\n\n" + "=" * 78 + "\n")
        out.write(f"{pkg['name']} {pkg['version']}  ({pkg.get('license') or 'see below'})\n")
        if pkg.get("repository"):
            out.write(pkg["repository"] + "\n")
        out.write("=" * 78 + "\n")
        files = sorted(f for f in os.listdir(folder) if f.lower().startswith(NAMES)
                       and os.path.isfile(os.path.join(folder, f)))
        if pkg.get("license_file"):
            files = [pkg["license_file"]] + [f for f in files if f != pkg["license_file"]]
        if not files:
            out.write(f"\nNo license file is included in the package. It is published\n"
                      f"under {pkg.get('license')}; the standard text applies.\n")
        for name in files:
            with open(os.path.join(folder, name), encoding="utf-8", errors="replace") as f:
                out.write(f"\n--- {name} ---\n\n{f.read().rstrip()}\n")


if __name__ == "__main__":
    main()
