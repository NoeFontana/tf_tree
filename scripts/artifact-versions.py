#!/usr/bin/env python3
"""One release, one version — and every recipe a document names has to exist.

`just artifact-versions`. This is the third gate of the family that starts with
`just msrv` and `scripts/evidence-audit.sh`, and it exists because the defect
those two catch showed up three more times while this release was being cut:
shipped text that contradicts a document the same text names as authoritative.

  * the README's status table against the `§0.0` tables it calls the source of
    truth;
  * the CLI's `--help`, still saying "live external attach arrives in Phase 2"
    two phases after it arrived;
  * `just py-wheel`, documented in the README as "build + install" while the
    recipe only built — so the documented quickstart ended in `ImportError`.

None of those was caught by a test, because none of them is a property of the
code. They are properties of the *repository*, and this file is where the ones
that can be checked exactly get checked.

**Exactly is the operative word.** `docs/PHASE5.md` §10 makes the point about
the benchmark baseline and it applies here: a gate that flaps is a gate people
learn to pass by editing the gate. So every rule below was measured against the
whole corpus before it was written down, and a rule with even one false
positive was narrowed until it had none. What that costs is coverage — the
README's status table is *not* checked here, because no cheap rule
distinguishes a stale row from a differently-worded true one. That check is a
human reading two tables, and pretending otherwise would be worse than the gap.

Run it from anywhere; it chdirs to the repository root.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

# `tomllib` is 3.11+, while `pyproject.toml` sets ruff's target to py310 —
# that floor is the *package's*, not this script's. Reported rather than left
# as a traceback, because the fix is "run this on a newer python3" and
# `ModuleNotFoundError: No module named 'tomllib'` does not say so.
try:
    import tomllib
except ModuleNotFoundError:
    print(
        f"artifact-versions: needs Python 3.11+ for tomllib; this interpreter is "
        f"{sys.version.split()[0]}",
        file=sys.stderr,
    )
    raise SystemExit(2) from None

ROOT = Path(__file__).resolve().parent.parent

# **The five crates this release publishes**, and the one literal in this file
# that is not read out of the repository. It is here rather than derived
# because there is nothing to derive it from: `publish` in a manifest is the
# *authority*, and a gate that reads the authority and compares it to itself
# checks nothing. README's "Workspace" section and `CHANGELOG.md` state this
# same set in prose; what fails here is a sixth crate quietly joining the
# release because somebody added a manifest without `publish = false`.
PUBLISHABLE = frozenset(
    {"tf_tree", "tf_tree_core", "tf_tree_math", "tf_tree_arena", "tf_tree_ipc"}
)

# Every file that carries a hand-kept copy of the version. Each one must yield
# at least one site: a scan that silently finds nothing is the classic way a
# gate keeps passing after the thing it looked at moved.
VERSION_FILES = (
    "Cargo.toml",
    "pyproject.toml",
    "crates/tf_tree_py/Cargo.toml",
    "crates/tf_tree_tf2_sys/Cargo.toml",
    "crates/tf_tree_c/CMakeLists.txt",
    "ros/tf_tree_ros/CMakeLists.txt",
    "ros/tf_tree_ros/package.xml",
    "ros/tf_tree_bench_ros/CMakeLists.txt",
    "ros/tf_tree_bench_ros/package.xml",
)

CMAKE_FILES = (
    "crates/tf_tree_c/CMakeLists.txt",
    "ros/tf_tree_ros/CMakeLists.txt",
    "ros/tf_tree_bench_ros/CMakeLists.txt",
)

PACKAGE_XML_FILES = (
    "ros/tf_tree_ros/package.xml",
    "ros/tf_tree_bench_ros/package.xml",
)

# The manifests outside `[workspace]` — they cannot inherit
# `[workspace.package] version`, so they spell it, and `cargo` never compares
# the copies. Same shape as the `rust-version` arm of `just msrv`, and the same
# reason: compared rather than compiled is the strongest check available for a
# crate this host may not be able to build.
EXCLUDED_MANIFESTS = (
    "crates/tf_tree_py/Cargo.toml",
    "crates/tf_tree_tf2_sys/Cargo.toml",
)

# `project(<name> VERSION <v> ...)`. Anchored on the keyword rather than on the
# line, because `cmake_minimum_required(VERSION 3.16)` is two lines above it in
# every one of these files and matching bare `VERSION` would read the wrong
# number and still pass.
PROJECT_VERSION_RE = re.compile(
    r"\bproject\s*\(\s*[A-Za-z0-9_]+\s+VERSION\s+([0-9][^\s)]*)"
)

failures: list[str] = []


def fail(message: str) -> None:
    failures.append(message)


def load_toml(rel: str) -> dict:
    with (ROOT / rel).open("rb") as handle:
        return tomllib.load(handle)


# ---------------------------------------------------------------------------
# 1. Every version string in the repository agrees.
# ---------------------------------------------------------------------------


def collect_version_sites(authority: str) -> list[tuple[str, str, str]]:
    """(file, where-in-it, value) for every hand-kept version in the repo.

    TOML is parsed rather than grepped, and so is `package.xml`. That is not
    fastidiousness: three of these files carry the version *in prose* in the
    comment directly above the field — `crates/tf_tree_c/CMakeLists.txt` names
    `find_package(tf_tree 0.0.1 CONFIG REQUIRED)` as an example — and a grep
    for the number would report those comments as sites, which means the next
    bump would fail the gate on an illustration rather than on a fact.
    """
    sites: list[tuple[str, str, str]] = []

    root = load_toml("Cargo.toml")

    # The intra-workspace pins. A publishable crate wired by path *alone* is
    # unpublishable — cargo refuses "dependency `x` does not specify a
    # version" — so a missing pin is a release-blocking defect and not just a
    # drift, and it is reported as its own failure rather than as a missing
    # site.
    for name, dep in sorted(root["workspace"]["dependencies"].items()):
        if not isinstance(dep, dict):
            continue
        path = dep.get("path", "")
        if not path.startswith("crates/"):
            continue
        version = dep.get("version")
        if version is None:
            if name in PUBLISHABLE:
                fail(
                    f"Cargo.toml [workspace.dependencies] {name}: wired by path with "
                    f"no version.\n    {name} is published, so cargo will refuse to "
                    f"package any crate that depends on it."
                )
            continue
        sites.append(
            ("Cargo.toml", f"[workspace.dependencies] {name}.version", version)
        )

    for manifest in EXCLUDED_MANIFESTS:
        parsed = load_toml(manifest)
        package_version = parsed["package"]["version"]
        if not isinstance(package_version, str):
            fail(
                f"{manifest} [package] version is {package_version!r}, not a string. "
                f"This crate is outside [workspace], so it cannot inherit."
            )
        else:
            sites.append((manifest, "[package] version", package_version))
        for name, dep in sorted(parsed.get("dependencies", {}).items()):
            if isinstance(dep, dict) and "path" in dep and "version" in dep:
                sites.append(
                    (manifest, f"[dependencies] {name}.version", dep["version"])
                )

    sites.append(
        (
            "pyproject.toml",
            "[project] version",
            load_toml("pyproject.toml")["project"]["version"],
        )
    )

    for rel in CMAKE_FILES:
        # Comment lines stripped for the same reason `evidence-audit.sh` strips
        # them: the comment above `project()` in all three of these files
        # quotes a version, and one of them quotes it inside a `find_package`
        # call that would match a laxer regex.
        body = "\n".join(
            line.split("#", 1)[0] for line in (ROOT / rel).read_text().splitlines()
        )
        found = PROJECT_VERSION_RE.findall(body)
        if len(found) != 1:
            fail(
                f"{rel}: expected exactly one `project(<name> VERSION <v>)`, found "
                f"{len(found)}: {found}"
            )
            continue
        sites.append((rel, "project(... VERSION ...)", found[0]))

    for rel in PACKAGE_XML_FILES:
        # ElementTree drops comments, so the "Hand-kept copy of the root
        # Cargo.toml's version" note above the tag cannot be read as a site.
        version = ET.fromstring((ROOT / rel).read_text()).findtext("version")
        if version is None:
            fail(f"{rel}: no <version> element")
            continue
        sites.append((rel, "<version>", version.strip()))

    return sites


def check_versions() -> str:
    root = load_toml("Cargo.toml")
    authority = root["workspace"]["package"]["version"]

    sites = collect_version_sites(authority)

    covered = {file for file, _, _ in sites}
    for rel in VERSION_FILES:
        if rel == "Cargo.toml":
            continue  # its own [workspace.package] version is the authority
        if rel not in covered:
            fail(
                f"{rel}: no version site found. Either the file stopped carrying one "
                f"(then drop it from VERSION_FILES here, in the same commit) or the "
                f"scan stopped seeing it, which is worse."
            )

    for file, where, value in sites:
        if value != authority:
            fail(
                f"{file} {where} = {value!r}\n"
                f"    the root Cargo.toml's [workspace.package] version is "
                f"{authority!r}. That field is the source of truth; every other copy "
                f"is hand-kept."
            )

    # `covered`, not `len(VERSION_FILES)`: the root `Cargo.toml` is both the
    # authority and a carrier — its five intra-workspace pins are hand-kept
    # copies like any other — so counting it out would understate by one.
    return (
        f"{len(sites)} hand-kept version sites in {len(covered)} files all read "
        f"{authority} — the root Cargo.toml's [workspace.package] version"
    )


# ---------------------------------------------------------------------------
# 2. The release publishes exactly the crates it says it publishes.
# ---------------------------------------------------------------------------


def check_publishable(authority: str) -> str:
    out = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    packages = json.loads(out)["packages"]

    # cargo spells `publish = false` as an empty allow-list and "publishable
    # anywhere" as null. Nothing here uses a registry allow-list; if one ever
    # appears this reads it as not-publishable, which is the safe direction.
    publishable = {p["name"] for p in packages if p.get("publish") is None}

    if publishable != set(PUBLISHABLE):
        extra = sorted(publishable - PUBLISHABLE)
        missing = sorted(PUBLISHABLE - publishable)
        detail = []
        if extra:
            detail.append(
                f"    would also be published: {', '.join(extra)} — add "
                f"`publish = false` with the reason in the manifest, or add it to "
                f"this gate's list and to README's Workspace section and "
                f"CHANGELOG.md in the same commit"
            )
        if missing:
            detail.append(
                f"    no longer publishable: {', '.join(missing)} — "
                f"the release names it"
            )
        fail(
            "the publishable set is not the five crates this release names.\n"
            + "\n".join(detail)
        )

    for p in packages:
        if p["version"] != authority:
            fail(
                f"{p['name']} is at {p['version']}, the workspace is at {authority}. "
                f"A workspace member that does not inherit [workspace.package] version "
                f"is a release with two numbers in it."
            )

    return f"the publishable set is exactly {', '.join(sorted(PUBLISHABLE))}"


# ---------------------------------------------------------------------------
# 3. The changelog has an entry for the version being released.
# ---------------------------------------------------------------------------


def check_changelog(authority: str) -> str:
    text = (ROOT / "CHANGELOG.md").read_text()
    if not re.search(rf"^## \[{re.escape(authority)}\]", text, re.M):
        fail(
            f"CHANGELOG.md has no `## [{authority}]` section.\n"
            f"    The version moved and the changelog did not. Keep a Changelog 1.1.0 "
            f"is the format the file declares."
        )
    return f"CHANGELOG.md has a section for {authority}"


# ---------------------------------------------------------------------------
# 4. Every `just <recipe>` a maintained document or a workflow names exists.
# ---------------------------------------------------------------------------

# A recipe name as this repository spells them: lowercase, digits, hyphens.
# Anything else in the argument position is deliberately *not* checked, and
# there are exactly two such tokens in the corpus today — `just --list` (a
# flag) and `just py-*` (prose about a family of recipes, not a call). Both are
# real writing and neither is a recipe reference; narrowing the rule to exclude
# them is what took this check from three findings to one.
RECIPE_TOKEN_RE = re.compile(r"[a-z][a-z0-9-]*")

# Root markdown, the phase specs, and the benchmark register. **`docs/decisions/`
# is deliberately out of scope** even though all of its `just` references
# resolve today: a `ready` decision record is a dated artifact — this release
# left `0019`'s "0.1.0 scope" prose alone for exactly that reason — so renaming
# a recipe must not force an edit to a historical document. The maintained
# documents are the ones a reader is told to trust today.
DOC_GLOBS = ("*.md", "docs/*.md", "docs/benchmarks/*.md")

FENCE_RE = re.compile(r"```[^\n]*\n(.*?)```", re.S)
INLINE_RE = re.compile(r"`([^`\n]+)`")
JUST_CALL_RE = re.compile(r"\bjust\s+(\S+)")


def code_spans(markdown: str) -> list[tuple[int, str]]:
    """(offset, text) for every fenced block and inline code span — and nothing else.

    Prose is excluded on purpose. "just build it" is English; `just build` is a
    claim about this file. Scanning prose would put the gate at the mercy of a
    sentence, which is how a checker earns its reputation for flapping.

    Offsets are into the original document so a finding can name a line. The
    fences are blanked to spaces rather than deleted before the inline pass —
    same length, newlines kept — because deleting them would shift every offset
    after the first code block and the line numbers would be quietly wrong.
    """
    spans = [(m.start(1), m.group(1)) for m in FENCE_RE.finditer(markdown)]
    blanked = FENCE_RE.sub(lambda m: re.sub(r"[^\n]", " ", m.group(0)), markdown)
    spans += [(m.start(1), m.group(1)) for m in INLINE_RE.finditer(blanked)]
    return spans


def check_recipe_references() -> str:
    recipes = set(
        subprocess.run(
            ["just", "--summary"], cwd=ROOT, capture_output=True, text=True, check=True
        ).stdout.split()
    )
    if not recipes:
        fail("`just --summary` listed no recipes; this check would pass trivially")

    doc_checked = 0
    doc_files = sorted({p for glob in DOC_GLOBS for p in ROOT.glob(glob)})
    # Sorted before reporting: `code_spans` yields every fenced block first and
    # then every inline span, so unsorted output walks one file's line 238
    # before its line 60 and reads like a bug in the checker.
    doc_findings: list[tuple[str, int, str]] = []
    for path in doc_files:
        rel = path.relative_to(ROOT)
        text = path.read_text()
        for offset, span in code_spans(text):
            for match in JUST_CALL_RE.finditer(span):
                token = match.group(1)
                if not RECIPE_TOKEN_RE.fullmatch(token):
                    continue
                doc_checked += 1
                if token not in recipes:
                    line = text.count("\n", 0, offset + match.start()) + 1
                    message = (
                        f"{rel}:{line}: names `just {token}`, which is not a "
                        f"recipe.\n    Either the recipe was renamed and the "
                        f"document was not, or the document describes something "
                        f"that was never written."
                    )
                    doc_findings.append((str(rel), line, message))
    for _, _, message in sorted(doc_findings):
        fail(message)

    wf_checked = 0
    # Both spellings. Every workflow here is `.yml` today; a `.yaml` one that
    # nothing scanned would be the silent half of this check.
    workflows = sorted(
        {
            p
            for pat in ("*.yml", "*.yaml")
            for p in ROOT.glob(f".github/workflows/{pat}")
        }
    )
    for path in workflows:
        rel = path.relative_to(ROOT)
        for lineno, line in enumerate(path.read_text().splitlines(), 1):
            # Inline `#` truncates the line. That can hide a reference (a false
            # negative) and cannot invent one, which is the direction to err in
            # for a gate wired into `just lint`.
            for match in JUST_CALL_RE.finditer(line.split("#", 1)[0]):
                token = match.group(1)
                if not RECIPE_TOKEN_RE.fullmatch(token):
                    continue
                wf_checked += 1
                if token not in recipes:
                    fail(
                        f"{rel}:{lineno}: runs `just {token}`, which is not a recipe.\n"
                        f"    CI mirrors these recipes 1:1 and has produced no "
                        f"run since 2026-07-23, so nothing else would notice."
                    )

    return (
        f"{doc_checked} `just <recipe>` references in {len(doc_files)} documents and "
        f"{wf_checked} in {len(workflows)} workflows all resolve"
    )


def main() -> int:
    authority = load_toml("Cargo.toml")["workspace"]["package"]["version"]
    lines = [
        check_versions(),
        check_publishable(authority),
        check_changelog(authority),
        check_recipe_references(),
    ]

    if failures:
        print(
            "artifact-versions: the repository disagrees with itself.\n",
            file=sys.stderr,
        )
        for message in failures:
            print(f"  {message}\n", file=sys.stderr)
        print(
            f"{len(failures)} disagreement(s). The root Cargo.toml's "
            f"[workspace.package] version is the source of truth for the number; "
            f"the justfile is the source of truth for the recipe names.",
            file=sys.stderr,
        )
        return 1

    for line in lines:
        print(f"artifact-versions: {line}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
