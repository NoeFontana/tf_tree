#!/usr/bin/env python3
"""Emit a CycloneDX 1.5 SBOM for what a release actually ships.

`docs/PHASE5.md` §10 asks for "SBOM per release". This writes one from
`cargo metadata`, rather than adding `cargo-cyclonedx` to CI, for the reason
`release.yml` already gives for not using `cargo-dist`: a generated artifact
cannot carry the argument for its own shape, and every other step in these
workflows does.

**Scope is the shipped graph, not the workspace.** `cargo metadata --no-deps`
would list only the members; a plain `cargo metadata` lists dev-dependencies and
build-dependencies of everything, which are not in any published artifact. The
resolve graph is walked from the release roots — the five publishable crates and
the CLI whose binaries are attached to the release — across `normal` edges only.
A dev-dependency is not shipped and does not belong in a bill of *materials*.

Deterministic by construction: no timestamp is read from the clock and no random
UUID is generated. The serial number is derived from the component set, so the
same graph yields the same document, and a diff between two releases is the
dependency change and nothing else.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys

# The crates whose contents reach a user: the five that publish, plus the CLI,
# whose binaries are attached to the GitHub Release.
ROOTS = (
    "tf_tree",
    "tf_tree_core",
    "tf_tree_math",
    "tf_tree_arena",
    "tf_tree_ipc",
    "tf_tree_cli",
)


def metadata() -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--all-features"],
        capture_output=True,
        text=True,
        check=True,
    )
    return json.loads(out.stdout)


def shipped_ids(md: dict) -> set[str]:
    """Package ids reachable from ROOTS over `normal` dependency edges."""
    nodes = {n["id"]: n for n in md["resolve"]["nodes"]}
    by_name = {p["name"]: p["id"] for p in md["packages"]}
    seen: set[str] = set()
    # **A root cargo cannot resolve is a REFUSAL, not a silent skip.** The
    # comprehension used to drop it, so a renamed or removed crate left a walk
    # starting from fewer roots — and with every root gone it emitted a
    # syntactically valid CycloneDX document with an empty `components` array at
    # exit 0. `scripts/artifact-versions.py` states the rule beside
    # `VERSION_FILES`: "a scan that silently finds nothing is the classic way a
    # gate keeps passing after the thing it looked at moved", and
    # `scripts/no-network.sh` implements it as refusing rather than skipping.
    missing = [r for r in ROOTS if r not in by_name]
    if missing:
        raise SystemExit(
            f"sbom: ROOTS names {', '.join(missing)}, which `cargo metadata` does not "
            f"resolve. The bill of materials would be a walk from the roots that are "
            f"left, and nothing downstream would say so."
        )
    stack = [by_name[r] for r in ROOTS]
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes.get(pid, {}).get("deps", []):
            # `dep_kinds` is empty only on very old cargo; treat that as normal.
            kinds = {k.get("kind") for k in dep.get("dep_kinds", [])} or {None}
            if None in kinds:  # `kind: null` is a normal dependency
                stack.append(dep["pkg"])
    return seen


def component(pkg: dict) -> dict:
    c = {
        "type": "library",
        "name": pkg["name"],
        "version": pkg["version"],
        "purl": f"pkg:cargo/{pkg['name']}@{pkg['version']}",
    }
    if pkg.get("license"):
        # CycloneDX wants one entry per licence; a SPDX expression goes in
        # `expression`, which is what cargo's `license` field actually is.
        c["licenses"] = [{"expression": pkg["license"]}]
    if pkg.get("description"):
        c["description"] = pkg["description"]
    return c


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True, help="the release version")
    ap.add_argument("-o", "--out", default="-")
    args = ap.parse_args()

    md = metadata()
    ids = shipped_ids(md)
    pkgs = sorted(
        (p for p in md["packages"] if p["id"] in ids),
        key=lambda p: (p["name"], p["version"]),
    )
    components = [component(p) for p in pkgs if p["name"] not in ROOTS]
    # The residual the refusal above does not cover: every root resolved and the
    # graph still came out empty. `tf_tree_core` alone pulls libm, bytemuck and
    # blake3, so this cannot happen while the dependency budget stands — which
    # is exactly why an empty document must be an error rather than a claim that
    # nothing ships.
    if not components:
        raise SystemExit(
            "sbom: the walk from ROOTS reached no dependency at all. An empty "
            "bill of materials is a broken traversal, not a claim that this "
            "release ships no third-party code."
        )

    digest = hashlib.sha256(
        "\n".join(f"{c['name']}@{c['version']}" for c in components).encode()
    ).hexdigest()
    # A UUID shaped from the content hash: same graph, same serial number.
    u = digest[:32]
    serial = f"urn:uuid:{u[:8]}-{u[8:12]}-{u[12:16]}-{u[16:20]}-{u[20:32]}"

    bom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": serial,
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "name": "tf_tree",
                "version": args.version,
                "purl": f"pkg:cargo/tf_tree@{args.version}",
            },
            "properties": [
                {"name": "tf_tree:roots", "value": " ".join(ROOTS)},
                {"name": "tf_tree:edge_kinds", "value": "normal"},
            ],
        },
        "components": components,
    }
    text = json.dumps(bom, indent=2, sort_keys=True) + "\n"
    if args.out == "-":
        sys.stdout.write(text)
    else:
        with open(args.out, "w", encoding="utf-8") as fh:
            fh.write(text)
        print(
            f"sbom: {len(components)} shipped dependencies -> {args.out}",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
