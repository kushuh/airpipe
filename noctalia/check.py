#!/usr/bin/env python3
"""Offline checks of the Noctalia plugin, for CI and releases (Noctalia is not
installed there; on a developer machine also run `noctalia plugins lint`):

* plugin.toml parses, has the required keys and a MAJOR.MINOR.PATCH version;
* every entry script exists;
* translations/en.json parses and has every string the manifest and the
  scripts look up: literal tr("...") keys, and the keys built at runtime
  listed in DYNAMIC.
"""

import json
import re
import sys
import tomllib
from pathlib import Path

PLUGIN = Path(__file__).resolve().parent / "airpipe"
ENTRY_KINDS = ("widget", "panel", "service", "shortcut")
# Keys the scripts build at runtime, as prefix -> values (keep in sync).
DYNAMIC = {
    "state": ["unknown", "missing", "too_old", "daemon_old", "error"],
    "cli": ["missing", "too_old", "daemon_old", "error"],
    "speaker": ["idle", "connecting", "streaming", "retrying", "in-use"],
    "latency": ["low-latency", "balanced", "robust"],
    "start": ["device-default", "last"],
}


def has(strings, key):
    node = strings
    for part in key.split("."):
        if not isinstance(node, dict) or part not in node:
            return False
        node = node[part]
    return isinstance(node, str)


def main():
    errors = []
    manifest = tomllib.loads((PLUGIN / "plugin.toml").read_text())
    for key in ("id", "name", "version", "plugin_api"):
        if key not in manifest:
            errors.append(f"plugin.toml: missing {key}")
    if not re.fullmatch(r"\d+\.\d+\.\d+", str(manifest.get("version", ""))):
        errors.append("plugin.toml: version must be MAJOR.MINOR.PATCH")

    entries = [entry for kind in ENTRY_KINDS for entry in manifest.get(kind, [])]
    for entry in entries:
        if not (PLUGIN / entry["entry"]).is_file():
            errors.append(f"plugin.toml: {entry['entry']} not found")

    strings = json.loads((PLUGIN / "translations" / "en.json").read_text())
    wanted = set()
    for setting in manifest.get("setting", []) + [s for e in entries for s in e.get("setting", [])]:
        wanted.update(k for k in (setting.get("label_key"), setting.get("description_key")) if k)
    for script in sorted(PLUGIN.glob("*.luau")):
        wanted.update(re.findall(r'\btr\("([^"]+)"', script.read_text()))
    for prefix, values in DYNAMIC.items():
        wanted.update(f"{prefix}.{value}" for value in values)
    errors += [f"translations/en.json: missing {key}" for key in sorted(wanted) if not has(strings, key)]

    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"{manifest['id']} {manifest['version']}: ok ({len(entries)} entries, {len(wanted)} strings)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
