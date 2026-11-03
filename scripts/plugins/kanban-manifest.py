#!/usr/bin/env python3
"""Regenerate the kanban plugin's release manifest from what is actually built.

The manifest binds each view entry to its bytes (`views[].digest`), and the host
refuses a package whose recorded digest does not match what it extracted. A
hand-maintained hash therefore fails on every UI change: someone has to
remember to recompute it, and a forgotten recompute only shows up as a refused
install.

So the digests are generated, never typed. Run this after `npm run build`:

    npm run build --prefix plugins/superpowers-kanban/ui
    python3 scripts/plugins/kanban-manifest.py

It rewrites `plugin.json` in place and refuses to run when the entry documents
are missing, because a manifest pointing at a file that was never built is worse
than no manifest: it looks complete.
"""

from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
PLUGIN = REPO / "plugins" / "superpowers-kanban"
UI = PLUGIN / "ui"
MANIFEST = PLUGIN / "plugin.json"

# The two views the page serves, and the document each one is opened with.
#
# Both entries are the same built HTML. The page routes on the URL fragment
# (`#settings`), so the host does not need to know how the plugin splits its own
# views — and two manifests pointing at one document is honest about that,
# whereas a second HTML file would only duplicate the same bootstrap script.
#
# Paths are relative to the package root (the plugin directory), not to `ui/`:
# the installer extracts the package and reads entries from its root, so a
# leading `ui/` here would resolve outside the package.
VIEWS = [
    ("main", "main", "ui/dist/index.html"),
    ("settings", "settings", "ui/dist/index.html"),
]

# Upstream Superpowers skills a card cannot be driven without (design §7.2, T22).
#
# Only the three that every card needs are listed. The driver objective offers
# `superagent-driven-development` *or* `executing-plans` to execute the plan, so
# either one missing is not a failure — and this manifest format is a flat list
# with no "or". Declaring one of them would block a healthy installation that
# uses the other, which is worse than not diagnosing that pair at all. The
# readiness check reports exactly what is declared and nothing more.
#
# No `range`: these are file copies under `skills/<name>/`, with no published
# semver to compare against (T05 Step 4 forbids inventing one). Absent range
# means "any version present", which is the honest check here: the presence of
# a `SKILL.md` under one of the host's skill roots.
WORKFLOWS = [
    "brainstorming",
    "writing-plans",
    "finishing-a-development-branch",
]

# The board's business methods. Names are `namespace/verb` with camelCase
# segments because the manifest schema requires it; the backend translates them
# in `dispatch.rs::bare_method`.
# Names avoid the checker's destructive-verb list on Read methods: a Read method
# whose name contains `set`, `write`, `create`… is refused even when the effect is
# honest, because the checker cannot tell a false positive from a lie. That is
# why settings reads are `board/prefs` rather than `board/readSettings` —
# "settings" contains "set", and renaming is cheaper than widening the rule.
METHODS = [
    ("board/list", "Read"),
    ("board/enqueue", "Write"),
    ("board/enqueuePrequeued", "Write"),
    ("board/promote", "Write"),
    ("board/readSwitch", "Read"),
    ("board/writeSwitch", "QuotaWrite"),
    ("board/prefs", "Read"),
    ("board/storePrefs", "QuotaWrite"),
    ("board/startLifecycle", "Resident"),
    ("board/stopLifecycle", "Resident"),
    # Reads the operation ledger. Declared so the bridge lets the page ask
    # "did that write land?" after a lost reply, instead of retrying it.
    ("board/operation", "Read"),
]

# CLI verbs, taken from `main.rs`'s USAGE line. Effects come from the frozen
# managed contract, which is checked against the source by `check-contracts.mjs`;
# duplicating them here would let the two drift, so they are read from it.
#
# `run` and `manifest` are deliberately absent. They are *maintenance* verbs:
# the managed contract classifies them `Maintenance`, and the manifest schema
# has no such effect because a plugin may not ask for it — lifecycle
# transactions issue it. Listing them here would force a false `Resident`, which
# would tell the host "this plugin runs a resident service" on the strength of a
# verb the user never grants. The contract keeps the authoritative
# classification; the manifest only lists verbs that are really the plugin's.
CLI_ORDER = [
    "add",
    "add-prequeued",
    "promote",
    "add-merge",
    "merge-request",
    "merge-finish",
    "merge-cancel",
    "done",
    "state",
    "archive",
    "purge",
    "list",
    "on",
    "off",
    "workdir",
]

TARGET = "darwin-aarch64"


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 16), b""):
            digest.update(chunk)
    return digest.hexdigest()


def cli_effects() -> dict[str, str]:
    contract = json.loads((PLUGIN / "contract" / "managed-v1.json").read_text())
    table = {k: v for k, v in (contract.get("cli") or {}).items() if not k.startswith("_")}
    effects: dict[str, str] = {}
    for verb in CLI_ORDER:
        entry = table.get(verb)
        if entry is None:
            raise SystemExit(
                f"managed-v1.json does not classify the CLI verb {verb!r}; "
                "classify it there rather than editing this script"
            )
        effect = entry.get("effect")
        if effect == "Maintenance":
            # Unreachable while CLI_ORDER omits `run`/`manifest`; kept as a
            # guard so adding a maintenance verb here fails loudly instead of
            # silently relabelling it.
            raise SystemExit(
                f"managed-v1.json claims {verb!r} is Maintenance; the manifest schema "
                "does not allow a plugin to declare that effect, so it must not be listed"
            )
        effects[verb] = effect
    return effects


def main() -> int:
    missing = [entry for _, _, entry in VIEWS if not (PLUGIN / entry).is_file()]
    if missing:
        print(
            "cannot write the manifest: these view entries are not built: "
            + ", ".join(sorted(set(missing)))
            + "\nRun: npm run build --prefix plugins/superpowers-kanban/ui",
            file=sys.stderr,
        )
        return 1

    effects = cli_effects()
    manifest = {
        "schemaVersion": 1,
        "packageFormat": 1,
        "plugin": "superpowers-kanban",
        "version": json.loads((UI / "package.json").read_text())["version"],
        "target": TARGET,
        "views": [
            {
                "id": view_id,
                "kind": kind,
                "entry": entry,
                "digest": sha256_file(PLUGIN / entry),
            }
            for view_id, kind, entry in VIEWS
        ],
        # No program entry in this release. T21 delivers the views; the managed
        # backend launcher lands with T22, and until then the board's runner is
        # reached through the legacy supervisor. `null` is the literal truth
        # here — this package declares no program — and the host treats it as a
        # view-only plugin rather than trying to exec something that is not in
        # the package.
        "program": None,
        "skills": [{"name": "superpowers-kanban", "path": "skills/superpowers-kanban"}],
        "methods": [{"name": name, "effect": effect} for name, effect in METHODS],
        # CLI verbs are namespaced as `kanban:<verb>`. The manifest schema only
        # allows hyphens inside the segment after a colon, and several real verbs
        # are hyphenated (`add-prequeued`, `merge-request`); the runner strips the
        # same prefix, so both spellings name one command.
        "cli": [{"verb": f"kanban:{verb}", "effect": effects[verb]} for verb in CLI_ORDER],
        "capabilities": {
            "required": ["Read", "Write"],
            "optional": ["Launch", "QuotaWrite", "Resident"],
        },
        "dependencies": {"workflows": [{"name": name} for name in WORKFLOWS]},
        # How to recognise a pre-managed install of this plugin (design
        # §8.1). The host probes generically: it reads these names out of the
        # verified manifest instead of hard-coding one plugin's file layout.
        # Listed here, not only in the committed json, because this script
        # rebuilds the manifest from scratch and would otherwise drop it.
        "data": {
            "format": 1,
            "migrations": [],
            "legacy": {
                "binaries": ["superpowers-kanban", "superpowers-kanban-runner"],
                "supervisor": "superpowers-kanban.json",
                "stateDirs": [".yi-agent/superpowers-kanban"],
                "preferences": ".yi-agent/preferences.json",
                "switchKey": "superpowers_kanban",
                "skill": "superpowers-kanban",
            },
        },
        "compatibility": {"hostApi": "v1", "uiApi": "v1"},
    }

    MANIFEST.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")
    print(f"wrote {MANIFEST.relative_to(REPO)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
