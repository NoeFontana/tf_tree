#!/usr/bin/env python3
"""One release, one version — and every recipe a document names has to exist.

`just artifact-versions`. This is the third gate of the family that starts with
`just msrv` and `scripts/evidence-audit.sh`, and it exists because the defect
those two catch showed up three more times while this release was being cut —
and once more after it: shipped text that contradicts a document the same text
names as authoritative, or that no reader can see at all.

  * the README's status table against the `§0.0` tables it calls the source of
    truth;
  * the CLI's `--help`, still saying "live external attach arrives in Phase 2"
    two phases after it arrived;
  * `just py-wheel`, documented in the README as "build + install" while the
    recipe only built — so the documented quickstart ended in `ImportError`;
  * and, later, two rows of `docs/PHASE2.md` §12.2 whose measured results
    rendered as *nothing* on github.com, because each carried them in a third
    cell of a two-column table (#208) — hiding a benchmark's whole answer and,
    behind it, a figure that had gone stale.

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
                        f"    CI mirrors these recipes 1:1, so this is a job that "
                        f"will fail on `error: Justfile does not contain recipe`."
                    )

    return (
        f"{doc_checked} `just <recipe>` references in {len(doc_files)} documents and "
        f"{wf_checked} in {len(workflows)} workflows all resolve"
    )


# ---------------------------------------------------------------------------
# 5. Every Markdown table row has as many cells as its header.
# ---------------------------------------------------------------------------

# GFM truncates a row to the header's column count and **says nothing**. A cell
# past the last column is not a rendering wart, it is deletion: it is gone on
# github.com while every local editor, every `less` and every diff still shows
# it, so the author who wrote it has no way to notice. Twice now that has hidden
# something that mattered:
#
#   * `docs/API.md` row 16 shipped with two unescaped pipes inside `|s| ≈ 2.3`
#     and split into seven cells of a five-column table. The commit that landed
#     the row said it had fixed exactly that, and had not; a review found it by
#     counting pipes by hand.
#   * `docs/PHASE2.md` §12.2 carried two three-cell rows in a two-column table
#     for five days (#208). Invisible in them were a benchmark's whole result
#     and a stale figure that a later change had split in two — a wrong number
#     no reader could see to challenge.
#
# **Escape-aware, because `\|` is legitimate inside a cell** and is how API.md's
# row was fixed. A naive `line.count("|")` false-positives on §3.6's
# `SHRINK\|GROW` row, which is well formed; that false positive is the reason
# the rule is written against a splitter rather than a count.
#
# Rows with too *few* cells are reported as well, even though GFM pads those and
# nothing is lost. A table whose rows disagree with its header is a table
# somebody edited without counting the columns, and the next such edit is the
# one that deletes.
#
# **The risk runs both ways, and the second direction is the dangerous one.** A
# false *negative* — a ragged row this walks past — leaves the corpus exactly
# where it was. A false *positive* blocks a correct document, and a gate that
# does that is a gate somebody switches off. Three constructions put a pipe
# table where GFM renders none, and each is guarded rather than argued away:
#
#   * a **setext H2** — a prose line containing a `|`, then a bare `---` under
#     it — is a heading. GFM will not read a delimiter row without a `|` in it,
#     so neither does this, and every table in the corpus has one. Unguarded,
#     the check calls a heading "1 cells in a 2-column table", and 59 Markdown
#     files are each one missing blank line away from writing that heading by
#     accident.
#   * a **4-space-indented code block** is code, and a table drawn in one
#     renders as text. Skipped — but the threshold is four spaces past the
#     innermost open list item's content, not four from the margin, because
#     `docs/decisions/0005` has a real, rendered table indented four spaces
#     inside item 11 of an ordered list. A blanket "skip four spaces" would
#     stop checking it.
#   * an **HTML comment** hides everything through `-->`, and `CHANGELOG.md`
#     opens one. Skipped.
#
# Each guard was verified by writing the construction it describes into a file
# and watching the check stay silent, then breaking a row inside a *rendered*
# table in the same file and watching it fire.

# A delimiter cell: `---`, `:--`, `--:`, `:-:`, any width.
TABLE_DELIM_CELL_RE = re.compile(r"^\s*:?-+:?\s*$")

# Only ``` and ~~~ fences, up to three leading spaces, as CommonMark has it.
MD_FENCE_RE = re.compile(r"^\s{0,3}(```|~~~)")

# A list marker, which is what moves the indented-code threshold: content of an
# item indented by the marker's own width is content, not code.
LIST_MARKER_RE = re.compile(r"^(\s*)(?:[-*+]|\d{1,9}[.)])(\s+)")

# A blockquote marker, possibly nested, stripped before anything else looks at
# the line. Seven documents put tables inside `>` blocks — `docs/API.md`'s
# inlining measurements are the largest — and those render as tables on
# github.com and truncate exactly like any other. Without this the check walks
# past all of them: `> |` makes the first cell `> `, which no delimiter pattern
# matches, so the table is never recognised at all.
QUOTE_PREFIX_RE = re.compile(r"^\s*(?:>\s?)+")


def table_cells(line: str) -> list[str]:
    r"""Split a table row on unescaped `|`, the way GFM does.

    A backslash consumes the character after it, so `\|` stays inside its cell.
    The leading and trailing pipes every table in this repository carries are
    decoration: GFM drops the empty edge cells they produce, and dropping them
    here is what makes a row written without them count the same.
    """
    cells: list[str] = []
    current: list[str] = []
    i = 0
    while i < len(line):
        if line[i] == "\\" and i + 1 < len(line):
            current.append(line[i : i + 2])
            i += 2
            continue
        if line[i] == "|":
            cells.append("".join(current))
            current = []
            i += 1
            continue
        current.append(line[i])
        i += 1
    cells.append("".join(current))
    if cells and not cells[0].strip():
        cells = cells[1:]
    if cells and not cells[-1].strip():
        cells = cells[:-1]
    return cells


def strip_html_comments(line: str, inside: bool) -> tuple[str, bool]:
    """Drop `<!-- ... -->` spans, carrying the open/closed state across lines.

    An HTML comment is raw HTML through its `-->`: whatever is inside it does
    not render, so a table drawn there is not a table. Returns what is left of
    the line and whether a comment is still open after it.
    """
    kept: list[str] = []
    i = 0
    while i < len(line):
        if inside:
            end = line.find("-->", i)
            if end < 0:
                break
            inside = False
            i = end + 3
            continue
        start = line.find("<!--", i)
        if start < 0:
            kept.append(line[i:])
            break
        kept.append(line[i:start])
        inside = True
        i = start + 4
    return "".join(kept), inside


def parsed_lines(text: str) -> list[str]:
    """Blank every line GFM will not read as Markdown, keeping line numbers.

    Fenced code, HTML comments and indented code come back as empty strings
    rather than being dropped, so a finding's line number is still the line
    number in the file. Blanking is also how each of them *ends* a table, which
    is what GFM does with them.

    The indented-code threshold is four spaces past the innermost open list
    item's content indent. Four from the margin would be wrong in exactly one
    place that exists today — `docs/decisions/0005`'s table inside item 11 of an
    ordered list — and wrong in the direction that stops checking a real table.
    Where the list tracking guesses, it guesses the marker into existence and
    so raises the floor, which skips *less* and checks *more*.
    """
    out: list[str] = []
    fenced = False
    commented = False
    list_indents: list[int] = []
    for raw in text.splitlines():
        line = QUOTE_PREFIX_RE.sub("", raw)
        if not commented and MD_FENCE_RE.match(line):
            fenced = not fenced
            out.append("")
            continue
        if fenced:
            out.append("")
            continue
        line, commented = strip_html_comments(line, commented)
        if not line.strip():
            out.append("")
            continue
        indent = len(line) - len(line.lstrip(" "))
        while list_indents and indent < list_indents[-1]:
            list_indents.pop()
        floor = list_indents[-1] if list_indents else 0
        if indent >= floor + 4:
            out.append("")
            continue
        marker = LIST_MARKER_RE.match(line)
        if marker:
            list_indents.append(len(marker.group(0)))
        out.append(line)
    return out


def scan_tables(text: str) -> tuple[int, int, list[tuple[int, int, int, str]]]:
    """(tables, body rows, findings) — findings are (line, found, expected, row).

    A table is recognised the way GFM recognises one: a header line followed by
    a delimiter line of the same width, where the delimiter row **carries a
    `|`**. That last clause is the whole difference between a one-column table
    and a setext `---` heading, and without it a heading over a prose line
    holding a pipe is reported as a one-cell row. Fenced blocks, HTML comments
    and indented code are skipped, so a document that *shows* a broken table as
    an example is not a finding — and the counts come back with the findings
    because a detector that quietly stops matching would otherwise report a
    clean corpus forever.
    """
    lines = parsed_lines(text)
    findings: list[tuple[int, int, int, str]] = []
    tables = 0
    rows = 0
    i = 0
    while i < len(lines):
        if "|" not in lines[i] or i + 1 >= len(lines):
            i += 1
            continue
        # A delimiter cell holds only `-`, `:` and spaces, so no backslash can
        # reach one and a plain `in` is exactly "an unescaped pipe" here.
        if "|" not in lines[i + 1]:
            i += 1
            continue
        delim = table_cells(lines[i + 1])
        if not delim or not all(TABLE_DELIM_CELL_RE.match(c) for c in delim):
            i += 1
            continue
        header = table_cells(lines[i])
        tables += 1
        # A delimiter row that does not match its own header is the same defect
        # one line earlier: GFM stops seeing a table at all, and the whole thing
        # renders as a paragraph full of pipes.
        if len(header) != len(delim):
            findings.append((i + 2, len(delim), len(header), lines[i + 1]))
        j = i + 2
        while j < len(lines) and lines[j].strip() and "|" in lines[j]:
            rows += 1
            found = len(table_cells(lines[j]))
            if found != len(header):
                findings.append((j + 1, found, len(header), lines[j]))
            j += 1
        i = j
    return tables, rows, findings


def check_markdown_tables() -> str:
    """**Every tracked Markdown file**, `docs/decisions/` included.

    Wider than the recipe scan above, and for a reason that does not contradict
    it. That one holds records out because a recipe rename must not force an
    edit to a dated document. A ragged row is not a rename: it is a defect the
    document had on the day it was written, and a record that renders with a
    cell missing is misleading today. The corpus was measured before the rule
    was written — across every tracked Markdown file the only findings were
    `PHASE2.md` §12.2's two rows, which #208 fixed.
    """
    listed = subprocess.run(
        ["git", "ls-files", "-z", "*.md"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    files = sorted(f for f in listed.split("\0") if f)
    if not files:
        fail("`git ls-files '*.md'` listed nothing; this check would pass trivially")

    tables = rows = 0
    for rel in files:
        found_tables, found_rows, findings = scan_tables(
            (ROOT / rel).read_text(encoding="utf-8")
        )
        tables += found_tables
        rows += found_rows
        for lineno, found, expected, row in findings:
            # Two different defects, and the advice differs. Too many cells is
            # the one that deletes; too few is a table somebody stopped
            # counting, which is how the next edit becomes the first kind.
            why = (
                "GFM drops every cell past the header count and warns nobody, "
                "so what is written past the last column renders as nothing at "
                "all on github.com while looking right in every editor. Widen "
                "the header, fold the cell into its neighbour, or escape the "
                "pipe as `\\|` if it is content."
                if found > expected
                else "GFM pads a short row, so nothing is lost here — but a "
                "table whose rows disagree with its header is one nobody is "
                "counting the columns of, and the next such edit is the one "
                "that deletes a cell. Add the missing cell."
            )
            fail(
                f"{rel}:{lineno}: {found} cells in a {expected}-column table.\n"
                f"    {row.strip()[:110]}\n"
                f"    {why}"
            )
    if not tables:
        fail("no Markdown table was recognised anywhere; the detector is broken")

    return (
        f"{rows} rows in {tables} Markdown tables across {len(files)} documents all "
        f"have their header's cell count"
    )


PROSE_VERSION_RE = re.compile(r"\bv?[0-9]+\.[0-9]+\.[0-9]+\b")


def check_front_page_versions() -> str:
    """No three-component version literal in prose on a crates.io front page.

    **What it cost to not have this.** Four of the five publishable crates'
    `README.md` — the pages crates.io renders — opened their Version section with
    ``**0.0.1, and `0.0.x` promises nothing.**`` while `[workspace.package]
    version` was `0.0.3`. Wrong on the front page of `tf_tree`, `tf_tree_core`,
    `tf_tree_arena` and `tf_tree_ipc` for three releases. `tf_tree_math` had hit
    it first and fixed it the right way — by deleting the number and recording
    why — and **that fix reached one crate of five**, which is the whole argument
    for a gate: a lesson written into prose only propagates if the next person
    reads the prose.

    **Scope is derived, not listed.** `PUBLISHABLE` names the release; each of
    those manifests names its own front page in `[package] readme`. A crate that
    publishes without a `readme` key is itself a failure, in the same shape as
    `check_versions`' "no version site found" — a scan that silently finds
    nothing is how a gate keeps passing after its subject moved. So this does not
    pay option 2's stated cost of encoding "these five files".

    **Why three components, and why inline code is exempt.** Both narrower than
    they look, and both were measured against the whole corpus before being
    written, per this module's standard:

    * "no version-shaped literal at all" fails **57** times on these five files
      today, every one correct: `MSRV is **1.87**`, `Apache-2.0`, and
      `tf_tree_math`'s SE(3) worked examples with their `0.0` and `0.15`.
    * a bare three-component rule still fails **9** times, and all 9 are
      deliberate — `tf_tree/README.md`'s worked example of cargo's caret
      semantics, and the past-tense sentence #236 added to record this very bug.
      A gate in that shape would demand deleting the sentence that documents it.
    * exempting inline code spans is not a loophole, because the defect was not
      in code. It was **bold** — `**0.0.1, and ...**`. Measured: 0 hits across
      the five files today, and exactly 4 against `abd2fd9^` (the tree #236
      fixed), on exactly the four defective files, with `tf_tree_math` silent.

    Fenced blocks stay in scope deliberately, at no cost today — no README
    carries a versioned install snippet — so a future ```toml``` block pinning
    `tf_tree = "0.0.4"` is covered rather than exempt.

    **What this buys today: nothing, and that is the honest description.** After
    #236 no tracked Markdown file makes a live claim about the current version.
    This is purely a regression guard. Its value is that #236's lesson stops
    depending on anybody re-reading it.
    """
    checked = []
    for name in sorted(PUBLISHABLE):
        manifest = f"crates/{name}/Cargo.toml"
        readme = load_toml(manifest).get("package", {}).get("readme")
        if not readme:
            fail(
                f"{manifest} publishes but declares no [package] readme, so this "
                f"check cannot find its crates.io front page. Either name the "
                f"file or explain here why the crate has none."
            )
            continue

        rel = f"crates/{name}/{readme}"
        text = (ROOT / rel).read_text(encoding="utf-8")

        # Blank the code, keep the prose — the inverse of `code_spans`, which is
        # why that helper is not reused here. Fences are blanked first (same
        # length, newlines kept) so the inline pass cannot match a backtick
        # inside one and so every offset still names the right line.
        fence_blanked = FENCE_RE.sub(lambda m: re.sub(r"[^\n]", " ", m.group(0)), text)
        prose = list(text)
        for span in INLINE_RE.finditer(fence_blanked):
            for i in range(span.start(), span.end()):
                prose[i] = " "
        prose = "".join(prose)

        for hit in PROSE_VERSION_RE.finditer(prose):
            line_no = text.count("\n", 0, hit.start()) + 1
            line = text.splitlines()[line_no - 1].strip()
            fail(
                f"{rel}:{line_no} states the version {hit.group(0)!r} in prose:\n"
                f"      {line}\n"
                f"    This file is rendered as {name}'s crates.io front page, and "
                f"nothing updates a number written there. Delete it and say why, "
                f"as crates/tf_tree_math/README.md does — or put it in backticks "
                f"if it is a worked example rather than a claim about this release."
            )
        checked.append(rel)

    return (
        f"no version literal in prose on any of the "
        f"{len(checked)} publishable crates' front pages"
    )


def check_distribution_name() -> str:
    """The PyPI distribution name, wherever it is written by hand.

    It is **not** the import name, and that split is the reason this check
    exists. `tf_tree` could not be registered on PyPI — it is refused as too
    close to the existing `tftree`, because the similarity check strips
    separators — so the distribution is `transform_tree` while the module stays
    `tf_tree` (`docs/decisions/0008`'s correction records the measurement).

    Two consequences a reader will not expect, and each is a place to drift:
    `importlib.metadata.version(...)` takes the *distribution* name, so
    `tests/python/test_version.py` must ask for `transform_tree`; and a wheel is
    named after the distribution, so the globs that unpack one must match it.
    Neither is checked by anything else — `test_version.py` would fail with
    `PackageNotFoundError` at some later date, and the justfile globs would
    silently select nothing and assert on an empty list.
    """
    dist = load_toml("pyproject.toml")["project"]["name"]

    probe = Path("tests/python/test_version.py").read_text(encoding="utf-8")
    for asked in re.findall(r"importlib\.metadata\.version\(\s*\"([^\"]+)\"", probe):
        if asked != dist:
            fail(
                f"tests/python/test_version.py asks importlib.metadata for "
                f'"{asked}", but pyproject.toml [project] name is "{dist}". '
                f"That query is what pins the wheel's version to the crate's."
            )

    justfile = Path("justfile").read_text(encoding="utf-8")
    # Executable lines only. A comment may legitimately quote a historical
    # filename — the recipe that unpacks a wheel carries one naming the
    # `tf_tree-0.1.0` wheel that existed before this rename — and a gate that
    # fails on prose about the past is a gate people edit the prose to silence.
    stale = [
        line.strip()
        for line in justfile.splitlines()
        if "target/wheels/" in line
        and "*" in line
        and not line.strip().startswith("#")
        and f"{dist}-" not in line
    ]
    for line in stale:
        fail(f'a justfile wheel glob does not name the distribution "{dist}": {line}')

    return f'the PyPI distribution is "{dist}"; the module it installs is tf_tree'


def main() -> int:
    authority = load_toml("Cargo.toml")["workspace"]["package"]["version"]
    lines = [
        check_versions(),
        check_publishable(authority),
        check_changelog(authority),
        check_recipe_references(),
        check_markdown_tables(),
        check_front_page_versions(),
        check_distribution_name(),
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
