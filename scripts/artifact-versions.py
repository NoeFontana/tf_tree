#!/usr/bin/env python3
"""One release, one version — and every recipe a document names has to exist.

`just artifact-versions`. Checks properties of the *repository* that no test
sees and that can be checked exactly: shipped text that contradicts the
document it names as authoritative, or that no reader can see (e.g. table cells
GFM deletes). Every rule was measured against the whole corpus first and
narrowed until it had no false positive; the README status table is not checked
because no cheap rule tells a stale row from a reworded true one.

Run it from anywhere; it chdirs to the repository root.
"""

from __future__ import annotations

import functools
import json
import re
import subprocess
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

# `tomllib` is 3.11+ (the package's floor is py310, not this script's): report it rather than a traceback.
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

# The five crates this release publishes: the one literal not read from the repository, because `publish` in a manifest is the authority a gate would otherwise compare to itself.
PUBLISHABLE = frozenset(
    {"tf_tree", "tf_tree_core", "tf_tree_math", "tf_tree_arena", "tf_tree_ipc"}
)

# Every file carrying a hand-kept copy of the version; each must yield at least one site.
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

# The manifests outside `[workspace]` cannot inherit `[workspace.package] version`, so they spell it and are compared.
EXCLUDED_MANIFESTS = (
    "crates/tf_tree_py/Cargo.toml",
    "crates/tf_tree_tf2_sys/Cargo.toml",
)

# `project(<name> VERSION <v> ...)`, anchored on the keyword: `cmake_minimum_required(VERSION 3.16)` precedes it.
PROJECT_VERSION_RE = re.compile(
    r"\bproject\s*\(\s*[A-Za-z0-9_]+\s+VERSION\s+([0-9][^\s)]*)"
)

failures: list[str] = []

# Measurements, as opposed to verdicts: a `check_*` summary asserts its rule and is withheld on failure; these print on every run.
notes: list[str] = []


def fail(message: str) -> None:
    failures.append(message)


def note(message: str) -> None:
    notes.append(message)


# One spelling of the `subprocess.run` boilerplate.
def _git(*args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout


# NUL-split so a path with a space survives; sorted; cached so two rules asking for `*.md` see one corpus.
@functools.cache
def tracked(*globs: str) -> tuple[str, ...]:
    listed = _git("ls-files", "-z", *globs)
    return tuple(sorted(f for f in listed.split("\0") if f))


def load_toml(rel: str) -> dict:
    with (ROOT / rel).open("rb") as handle:
        return tomllib.load(handle)


# 1. Every version string in the repository agrees.


def local_crate_names() -> frozenset[str]:
    """Every package name this repository defines, read out of the manifests.

    Derived rather than listed: `cargo metadata` cannot answer it, since
    `tf_tree_py`/`tf_tree_tf2_sys` are excluded from the workspace.
    """
    names = set()
    for rel in tracked("*Cargo.toml"):
        package = load_toml(rel).get("package", {})
        name = package.get("name")
        if isinstance(name, str):
            names.add(name)
    if not names:
        fail("no [package] name found in any tracked Cargo.toml; the scan is broken")
    return frozenset(names)


@functools.cache
def tracked_lockfiles() -> tuple[str, ...]:
    """Every tracked `Cargo.lock`, read out of git rather than listed here.

    Generated, so they drift: cargo rewrites a path dependency's lock version
    silently, and nothing builds the excluded crates `--locked`
    (`crates/tf_tree_tf2_sys/Cargo.lock` sat at `0.0.1` for four releases).
    Derived so a fourth lockfile cannot fall outside the gate.
    """
    found = tracked("*Cargo.lock")
    if not found:
        fail("no tracked Cargo.lock found; the scan is broken")
    return found


def collect_version_sites(authority: str) -> list[tuple[str, str, str]]:
    """(file, where-in-it, value) for every hand-kept version in the repo.

    TOML and `package.xml` are parsed, not grepped: comments above the field
    quote versions and would be read as sites.
    """
    sites: list[tuple[str, str, str]] = []

    root = load_toml("Cargo.toml")

    # The intra-workspace pins: a publishable crate wired by path alone is unpublishable, so a missing pin is its own failure.
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
        # Comment lines stripped (as `evidence-audit.sh` does): the comment above `project()` quotes a version.
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
        # ElementTree drops comments, so the note above the tag is not a site.
        version = ET.fromstring((ROOT / rel).read_text()).findtext("version")
        if version is None:
            fail(f"{rel}: no <version> element")
            continue
        sites.append((rel, "<version>", version.strip()))

    # A lockfile lists every package in the graph, so entries are filtered by name
    # (packages this repository defines) and by the absence of `source` (a path
    # package). A path `[patch]` to another checkout has no `source` key, so it is
    # read as ours and compared.
    ours = local_crate_names()
    for rel in tracked_lockfiles():
        for package in load_toml(rel).get("package", []):
            name = package.get("name")
            if name not in ours or "source" in package:
                continue
            sites.append((rel, f"[[package]] {name}.version", package["version"]))

    return sites


def check_versions() -> str:
    root = load_toml("Cargo.toml")
    authority = root["workspace"]["package"]["version"]

    sites = collect_version_sites(authority)

    covered = {file for file, _, _ in sites}
    for rel in VERSION_FILES + tracked_lockfiles():
        if rel == "Cargo.toml":
            continue  # its own [workspace.package] version is the authority
        if rel not in covered:
            fail(
                f"{rel}: no version site found. Either the file stopped carrying one "
                f"(a manifest: drop it from VERSION_FILES here, in the same commit; a "
                f"lockfile: it no longer records any package this repository "
                f"defines, which wants an argument) or the scan stopped seeing it, "
                f"which is worse."
            )

    for file, where, value in sites:
        if value != authority:
            fail(
                f"{file} {where} = {value!r}\n"
                f"    the root Cargo.toml's [workspace.package] version is "
                f"{authority!r}. That field is the source of truth; every other copy "
                f"is hand-kept."
            )

    # `covered`, not `len(VERSION_FILES)`: the root `Cargo.toml` is both authority and carrier.
    return (
        f"{len(sites)} version sites in {len(covered)} files all read "
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

    # `publish = false` is an empty allow-list, "anywhere" is null; a registry allow-list reads as not-publishable (the safe direction).
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
    archived = ROOT / "docs" / "changelog" / f"{authority}.md"
    text = (ROOT / "CHANGELOG.md").read_text()
    if archived.is_file():
        text += "\n" + archived.read_text()
    if not re.search(rf"^## \[{re.escape(authority)}\]", text, re.M):
        fail(
            f"Neither CHANGELOG.md nor docs/changelog/{authority}.md has a "
            f"`## [{authority}]` section.\n"
            f"    The version moved and the changelog did not. Keep a Changelog 1.1.0 "
            f"is the format the file declares."
        )
    return f"CHANGELOG.md has a section for {authority}"


# ---------------------------------------------------------------------------
# 4. Every `just <recipe>` a maintained document or a workflow names exists.
# ---------------------------------------------------------------------------

# A recipe name: lowercase, digits, hyphens. Anything else in the argument
# position (a flag, a glob like `just py-*`, a metavariable, a token carrying a
# trailing backtick) is deliberately not checked.
#
# Skipped real reference: `.github/workflows/nightly.yml`'s `just ${{ matrix.recipe }}`
# (`tsan`, `shm-torture-asan`); both are named in the maintained documents and
# checked by the doc arm.
RECIPE_TOKEN_RE = re.compile(r"[a-z][a-z0-9-]*")

# Root markdown, the phase specs and the benchmark register. `docs/decisions/` is
# out: a dated record must not be edited for a recipe rename.
DOC_GLOBS = ("*.md", "docs/*.md", "docs/benchmarks/*.md")

FENCE_RE = re.compile(r"```[^\n]*\n(.*?)```", re.S)
INLINE_RE = re.compile(r"`([^`\n]+)`")
JUST_CALL_RE = re.compile(r"\bjust\s+(\S+)")


def code_spans(markdown: str) -> list[tuple[int, str]]:
    """(offset, text) for every fenced block and inline code span — and nothing else.

    Prose is excluded: "just build it" is English, `just build` is a claim.
    Offsets index the original document; fences are blanked to spaces (same
    length) before the inline pass so line numbers stay right.
    """
    spans = [(m.start(1), m.group(1)) for m in FENCE_RE.finditer(markdown)]
    blanked = FENCE_RE.sub(lambda m: re.sub(r"[^\n]", " ", m.group(0)), markdown)
    spans += [(m.start(1), m.group(1)) for m in INLINE_RE.finditer(blanked)]
    return spans


def blank_code(markdown: str, *, fences: bool = True) -> str:
    """`code_spans`' inverse: the document with its code blanked to spaces.

    Same length, newlines kept. The flag chooses whether *fenced* blocks are
    blanked: a link in a ```sh``` block is not a link, but a version in a fence
    on a front page is the claim that check looks for.
    """
    blanked = FENCE_RE.sub(lambda m: re.sub(r"[^\n]", " ", m.group(0)), markdown)
    keep = blanked if fences else markdown
    out = list(keep)
    for span in INLINE_RE.finditer(blanked):
        for i in range(span.start(), span.end()):
            out[i] = " "
    return "".join(out)


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
    # Sorted: `code_spans` yields fences first, then inline spans.
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
    # Anti-vacuity, in the shape siblings use (`if not X`, never a magnitude): catches a scan that stopped matching.
    if not doc_files:
        fail(f"DOC_GLOBS {DOC_GLOBS} matched no document; this check scanned nothing")
    if not doc_checked:
        fail(
            "no `just <recipe>` reference was found in any maintained document.\n"
            "    Every one of them resolving is not what that means — the "
            "detector stopped matching."
        )

    wf_checked = 0
    # Both `.yml` and `.yaml`.
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
            # Inline `#` truncates the line: may hide a reference, cannot invent one.
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

    if not workflows:
        fail(
            ".github/workflows/ matched no `.yml` or `.yaml` file; the workflow "
            "half of this check scanned nothing"
        )
    if not wf_checked:
        fail(
            "no `just <recipe>` reference was found in any workflow.\n"
            "    CI mirrors these recipes 1:1 by invoking them, so a workflow "
            "corpus with no `just` call in it means this scan stopped matching."
        )

    return (
        f"{doc_checked} `just <recipe>` references in {len(doc_files)} documents and "
        f"{wf_checked} in {len(workflows)} workflows all resolve"
    )


# ---------------------------------------------------------------------------
# 5. Every Markdown table row has as many cells as its header.
# ---------------------------------------------------------------------------
#
# GFM truncates a row to the header's width and says nothing, so a cell past the
# last column is deleted invisibly (#208). Rows with too few cells are reported
# too. The splitter is escape-aware (`\|` is legitimate in a cell). A false
# positive blocks a correct document, so three constructions GFM does not render
# as tables are guarded:
#
#   * a setext H2 (prose containing `|`, then a bare `---`): a delimiter row must contain a `|`;
#   * a 4-space-indented code block: the threshold is four past the innermost
#     open list item's content, not the margin (`docs/decisions/0005`);
#   * an HTML comment, through `-->`.

# A delimiter cell: `---`, `:--`, `--:`, `:-:`, any width.
TABLE_DELIM_CELL_RE = re.compile(r"^\s*:?-+:?\s*$")

# Only ``` and ~~~ fences, up to three leading spaces, as CommonMark has it.
MD_FENCE_RE = re.compile(r"^\s{0,3}(```|~~~)")

# A list marker (moves the indented-code threshold).
LIST_MARKER_RE = re.compile(r"^(\s*)(?:[-*+]|\d{1,9}[.)])(\s+)")

# A blockquote marker, possibly nested, stripped first: tables inside `>` blocks render and truncate like any other.
QUOTE_PREFIX_RE = re.compile(r"^\s*(?:>\s?)+")


def table_cells(line: str) -> list[str]:
    r"""Split a table row on unescaped `|`, the way GFM does.

    A backslash consumes the next character, so `\|` stays in its cell; the empty
    edge cells from leading and trailing pipes are dropped.
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

    Returns what is left of the line and whether a comment is still open.
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

    Fenced code, HTML comments and indented code come back empty, which also ends
    a table. The indented-code threshold is four spaces past the innermost open
    list item's content indent (`docs/decisions/0005` has a table inside a list item).
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

    A table is a header line plus a delimiter line of the same width that
    **carries a `|`** (else it is a setext heading). Fences, HTML comments and
    indented code are skipped. Counts come back so a matcher that stopped
    matching cannot report a clean corpus.
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
        # A delimiter cell holds no backslash, so a plain `in` is "an unescaped pipe".
        if "|" not in lines[i + 1]:
            i += 1
            continue
        delim = table_cells(lines[i + 1])
        if not delim or not all(TABLE_DELIM_CELL_RE.match(c) for c in delim):
            i += 1
            continue
        header = table_cells(lines[i])
        tables += 1
        # A delimiter row that does not match its header: GFM renders no table.
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

    Wider than the recipe scan: a ragged row is a defect the document had on the
    day it was written, not a rename a dated record should not track.
    """
    files = tracked("*.md")
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
            # Too many cells deletes; too few is a table somebody stopped counting.
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


# `](target` — an inline link's destination up to the first whitespace or `)`. Reference definitions are out of scope.
MD_LINK_RE = re.compile(r"\]\(\s*([^)\s]+)")


def check_relative_links() -> str:
    """Every relative Markdown link resolves to a file that exists.

    Code is blanked first, fences included (unlike `check_front_page_versions`):
    a path in a ```sh``` block is an argument. Anchors are not checked; a
    `#fragment` is stripped before resolving. The floor fails a scan that matched
    nothing.
    """
    files = tracked("*.md")

    checked = 0
    for rel in files:
        text = (ROOT / rel).read_text(encoding="utf-8")
        prose = blank_code(text)
        for hit in MD_LINK_RE.finditer(prose):
            target = hit.group(1)
            # A scheme before the first `/`, or a bare in-page anchor: not a file.
            head = target.split("/", 1)[0]
            if ":" in head or target.startswith("#"):
                continue
            checked += 1
            path = target.split("#", 1)[0]
            if not path:
                continue
            resolved = (ROOT / rel).parent / path
            if resolved.exists():
                continue
            line_no = text.count("\n", 0, hit.start()) + 1
            fail(
                f"{rel}:{line_no} links to {target!r}, which does not exist.\n"
                f"      resolved: {resolved.resolve()}\n"
                f"    Relative links are resolved against the file that carries "
                f"them, so a `../` from inside `docs/` leaves the repository."
            )

    if checked < 100:
        fail(
            f"only {checked} relative links were found across {len(files)} "
            f"documents. This corpus has hundreds; a scan that stops matching "
            f"reports every document as clean."
        )

    return f"{checked} relative links in {len(files)} documents all resolve"


PROSE_VERSION_RE = re.compile(r"\bv?[0-9]+\.[0-9]+\.[0-9]+\b")


def check_front_page_versions() -> str:
    """No three-component version literal in prose on a crates.io front page.

    Each crate's front page is named by its manifest's `readme`; the root
    `README.md` is also checked (PyPI's, per `pyproject.toml`). A crate or
    project with no `readme` key fails. Inline code is exempt (the defect was
    bold text); fenced blocks stay in scope. A regression guard: no page makes a
    live version claim today.
    """
    # (index name, front page) for the five crates.io pages, then the PyPI one.
    pages: list[tuple[str, str]] = []
    for name in sorted(PUBLISHABLE):
        manifest = f"crates/{name}/Cargo.toml"
        readme = load_toml(manifest).get("package", {}).get("readme")
        if not isinstance(readme, str) or not readme:
            fail(
                f"{manifest} publishes but declares no [package] readme, so this "
                f"check cannot find its crates.io front page. Either name the "
                f"file or explain here why the crate has none."
            )
            continue
        pages.append((name, f"crates/{name}/{readme}"))

    project = load_toml("pyproject.toml")["project"]
    py_readme = project.get("readme")
    if not isinstance(py_readme, str) or not py_readme:
        fail(
            "pyproject.toml [project] declares no `readme` (or declares one this "
            "check cannot resolve to a path), so the PyPI front page for "
            f"{project.get('name', '?')!r} is unknown here — and a distribution "
            "with no readme renders no description on the index at all."
        )
    else:
        pages.append((project["name"], py_readme))

    checked = []
    for name, rel in pages:
        text = (ROOT / rel).read_text(encoding="utf-8")

        # Blank inline code, keep prose and fences (see `blank_code`'s flag).
        prose = blank_code(text, fences=False)

        for hit in PROSE_VERSION_RE.finditer(prose):
            line_no = text.count("\n", 0, hit.start()) + 1
            line = text.splitlines()[line_no - 1].strip()
            fail(
                f"{rel}:{line_no} states the version {hit.group(0)!r} in prose:\n"
                f"      {line}\n"
                f"    This file is rendered as {name}'s front page on the index "
                f"that ships it, and nothing updates a number written there. "
                f"Delete it and say why, as crates/tf_tree_math/README.md does "
                f"— or put it in backticks "
                f"if it is a worked example rather than a claim about this release."
            )
        checked.append(rel)

    return (
        f"no version literal in prose on any of the {len(checked)} package-index "
        f"front pages (crates.io and PyPI)"
    )


def check_distribution_name() -> str:
    """The PyPI distribution name, wherever it is written by hand.

    It is not the import name: the distribution is `transform_tree`, the module
    stays `tf_tree` (`docs/decisions/0008`). `importlib.metadata.version(...)` and
    wheel globs take the distribution name.
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
    # Executable lines only: a comment may quote a historical filename.
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


DECISION_SETTLED_VERB = re.compile(
    r"(?i)\b(?:declin|amend|supersed|retir|settl|withdraw|govern|remov)\w*"
    r"[*_`]{0,2}\s+by\s+[*_`]{0,2}\[?[*_`]{0,2}(\d{4})[*_`]{0,2}\]?"
)

# Leading continuation markers stripped before joining, so a citation split across a blockquote or `//!` block is seen.
_CONTINUATION = re.compile(r"^\s*(?:>|//!|///|//|#)+\s?")


def check_decision_status_citations() -> str:
    """A `draft` decision record may not be cited as settled.

    A draft authorises nothing (`docs/decisions/README.md`). Matches
    `<verb> by <NNNN>` after normalising continuation markers and Markdown
    emphasis. Does NOT see a verb after the link, a dash form or bare adjacency;
    those stay held by review.
    """
    statuses: dict[str, str] = {}
    for path in sorted((ROOT / "docs" / "decisions").glob("0*.md")):
        for line in path.read_text(encoding="utf-8").splitlines():
            if line.startswith("**Status:**"):
                word = line[len("**Status:**") :].strip().split()
                if word:
                    statuses[path.name[:4]] = word[0].strip("`*").lower()
                break
    if not statuses:
        fail("no decision record statuses were read; this check would pass trivially")
        return "decision-status citations: not checked"

    files = tracked("*.md", "*.rs", "*.toml", "*.sh", "*.py", "*.yml", "justfile")
    if not files:
        fail("`git ls-files` listed no files for the decision-citation scan")
        return "decision-status citations: not checked"

    settled = 0
    for rel in files:
        # A record may discuss its own state only; the skip is per match, against this file's own id.
        own = rel.rsplit("/", 1)[-1][:4] if rel.startswith("docs/decisions/") else None
        try:
            raw = (ROOT / rel).read_text(encoding="utf-8").splitlines()
        except (UnicodeDecodeError, OSError):
            continue
        # Join into one normalised string, keeping an offset -> line-number map
        # so a finding can be printed as file:line.
        parts, offsets, pos = [], [], 0
        for n, line in enumerate(raw, 1):
            piece = _CONTINUATION.sub("", line).strip()
            parts.append(piece)
            offsets.append((pos, n))
            pos += len(piece) + 1
        joined = " ".join(parts)
        for m in DECISION_SETTLED_VERB.finditer(joined):
            rec = m.group(1)
            if rec == own:
                continue
            status = statuses.get(rec)
            if status is None:
                continue
            if status == "draft":
                line_no = next(
                    (n for off, n in reversed(offsets) if off <= m.start()), 1
                )
                fail(
                    f"{rel}:{line_no} cites `{rec}` as settled "
                    f'("{m.group(0).strip()}") but that record is `draft`. '
                    f"A draft authorises nothing — promote the record or soften "
                    f"the citation (docs/decisions/README.md, Lifecycle)."
                )
            else:
                settled += 1

    # Anti-vacuity floor, set just under the live count; raise it with the count.
    floor = 25
    if settled < floor:
        fail(
            f"the decision-citation scan found only {settled} settled citations "
            f"on non-draft records (floor {floor}); the pattern or the corpus "
            f"moved and this check is no longer looking at its subject"
        )

    return (
        f"{settled} settled decision-record citations across {len(files)} tracked "
        f"files, none of them on a `draft` record"
    )


# ---------------------------------------------------------------------------
# 10. The changelog is not behind the code a user can observe.
# ---------------------------------------------------------------------------

# Paths whose contents reach somebody who never clones this repository (shipping inside something counts, not `publish = false`).
RELEASE_VISIBLE = (
    "crates/tf_tree/src/",
    "crates/tf_tree_arena/src/",
    "crates/tf_tree_core/src/",
    "crates/tf_tree_ipc/src/",
    "crates/tf_tree_math/src/",
    "crates/tf_tree_bridge/src/",
    "crates/tf_tree_c/src/",
    "crates/tf_tree_c/include/",
    "crates/tf_tree_cli/src/",
    "crates/tf_tree_ingest/src/",
    "crates/tf_tree_py/src/",
    "python/",
    # A `find_package(tf_tree CONFIG)` consumer installs these; `cmake-check`
    # exists because three packaging defects hid in them.
    "crates/tf_tree_c/CMakeLists.txt",
    "crates/tf_tree_c/cmake/",
    # The rclcpp bridge node ROS users build against. #322 touched only `ros/`.
    "ros/",
)

# Reported in the summary line: a silently unfailable override is what `0023` step 5 is about.
NO_CHANGELOG = "[no changelog]"


def strip_fenced_blocks(text: str) -> tuple[str, bool]:
    """Blank fenced code blocks, recognising a fence only at the start of a line.

    A triple backtick inside an inline span is not a fence (a regex spelling
    inverted the pairing for the rest of the file). A fence inside a blockquote
    counts. An unclosed fence is reported through the returned flag, so the
    caller does not read the blanked tail as shed citations.
    """
    out: list[str] = []
    opened = 0
    for line in text.split("\n"):
        stripped = line.lstrip()
        while stripped.startswith(">"):
            stripped = stripped[1:].lstrip()
        run = len(stripped) - len(stripped.lstrip("`"))
        if run >= 3:
            # CommonMark: a fence closes only on a run at least as long as the one that opened it.
            if opened == 0:
                opened = run
                out.append("")
                continue
            if run >= opened:
                opened = 0
                out.append("")
                continue
        out.append("" if opened else line)
    return "\n".join(out), opened != 0


def check_line_citations() -> str:
    """`path.rs:LINE` citations in Markdown may not increase, per file.

    A ratchet: existing citations are grandfathered per file in
    `scripts/line-citation-budget.txt`, each row an equality (over or under
    fails), and the count may only fall. Cite a symbol instead. The pattern sees
    only the prefixed form (`crates/`, `xtask/`, `scripts/`, `ros/`); both totals
    print on every run. Fenced blocks are excluded, code spans are not.
    """
    budget_path = "scripts/line-citation-budget.txt"
    budget: dict[str, int] = {}
    for raw in Path(budget_path).read_text().splitlines():
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        count, name = raw.split("\t", 1)
        budget[name] = int(count)

    pattern = re.compile(
        r"\b(?:crates|xtask|scripts|ros)/[A-Za-z0-9_./-]+\.(?:rs|py):\d+"
    )
    files = tracked("*.md")
    found: dict[str, int] = {}
    # The same citation with the prefix optional (the form `CLAUDE.md` names), counted in the same pass.
    bare = re.compile(r"\b[A-Za-z0-9_][A-Za-z0-9_./-]*\.(?:rs|py):\d+")

    total = 0
    wider = 0
    unreadable: set[str] = set()
    for rel in files:
        prose, unclosed = strip_fenced_blocks(Path(rel).read_text(errors="replace"))
        if unclosed:
            unreadable.add(rel)
            fail(
                f"{rel} ends inside a fenced block — an unclosed ``` blanks the "
                f"rest of the file, so every `path.rs:LINE` citation after it "
                f"goes uncounted and the budget passes on a partial read"
            )
        hits = len(pattern.findall(prose))
        wider += len(bare.findall(prose))
        if hits:
            found[rel] = hits
            total += hits

    for rel, hits in sorted(found.items()):
        allowed = budget.get(rel, 0)
        if hits > allowed:
            fail(
                f"{rel} carries {hits} `path.rs:LINE` citation(s), over its "
                f"budget of {allowed} in {budget_path}. A line number breaks on "
                f"the next edit to the file it names — cite a symbol instead. "
                f"The budget may only fall; raising it needs a reason this "
                f"message cannot give you."
            )

    # A file with an unclosed fence reports a partial count; it already fails above.
    dropped = sorted(
        (rel, budget[rel], found.get(rel, 0))
        for rel in budget
        if rel not in unreadable and found.get(rel, 0) < budget[rel]
    )
    gone = sorted(rel for rel, _, _ in dropped if rel not in files)

    # An empty scan is not a pass: the equality above fails when the pattern stops matching. Assert the pattern's shape: it matches the prefixed form and not the bare form (the documented scope limit).
    if not pattern.search("see `crates/tf_tree/src/tree.rs:2182` for it"):
        fail(
            "the `path.rs:LINE` pattern no longer matches a full-path citation; "
            "this gate is asserting nothing"
        )
    if pattern.search("see `tree.rs:2182` for it"):
        fail(
            "the `path.rs:LINE` pattern now matches the bare form; that is a "
            "wider scope than every row in the budget was measured against, so "
            "the rows must be re-derived in the same commit"
        )

    # A drop is a failure, not a note: a row must be brought down in the commit that shed the citations.
    if dropped:
        shed = sum(was - now for _, was, now in dropped)
        listed = ", ".join(f"{rel} {was}->{now}" for rel, was, now in dropped)
        # Two remedies: a row for a file that no longer exists has nothing to lower.
        remedy = f"lower them in {budget_path} in the same commit"
        if gone:
            remedy += (
                f" — and {', '.join(gone)} is no longer tracked, so its row is "
                f"to be deleted rather than lowered"
            )
        fail(
            f"{len(dropped)} budget row(s) sit above their file, by {shed} "
            f"citation(s) in total — either the citations were removed and the "
            f"rows were not, or a row was written too high; both read the same "
            f"from here. {remedy}: {listed}. A row above its file is headroom "
            f"for a citation nobody had to justify."
        )
    # The uncounted half is printed, not written down; `bare` is a superset of `pattern`.
    if wider < total:
        fail(
            f"the wider citation census counted {wider} against the gated "
            f"{total}, and it cannot be smaller — every prefixed citation is "
            f"also a bare one, so the `bare` pattern has stopped matching"
        )
    if not bare.search("see `tree.rs:2182` for it"):
        fail(
            "the wider census pattern no longer matches a bare citation; the "
            "figure it prints is asserting nothing"
        )
    # A file whose fence never closed is read only to the fence, so both totals are partial.
    partial = (
        f" — PARTIAL: {len(unreadable)} file(s) were read only as far as an "
        f"unclosed fence, so both totals are short"
        if unreadable
        else ""
    )
    note(
        f"citation census: {total} `path.rs:LINE` in {len(found)} documents with "
        f"a directory prefix, {wider} with the prefix made optional — the gate "
        f"holds the first number, so its rows are a floor on the rot and not a "
        f"census of it{partial}. **Tracked Markdown only**: the same citations "
        f"are written in Rust doc comments, which nothing here scans"
    )
    return (
        f"{total} `path.rs:LINE` citations in {len(found)} documents, "
        f"each matching its row in {budget_path}"
    )


def check_changelog_freshness() -> str:
    """`CHANGELOG.md` must not sit behind a release-visible commit.

    An ordering, not a per-commit rule: no commit since the last `v*` tag
    touching a shipped path may land after the newest `CHANGELOG.md` commit. A
    dirty `CHANGELOG.md` counts as current. Does NOT prove the entries are
    correct, only that the file was edited after the last observable change.
    Reads paths, not diffs; reports *not checked* without a `v*` tag.
    """
    try:
        tag = _git("describe", "--tags", "--abbrev=0", "--match", "v*", "HEAD").strip()
    except subprocess.CalledProcessError:
        return "changelog freshness: NOT CHECKED — no `v*` tag is reachable from HEAD"

    rng = f"{tag}..HEAD"
    order = {c: i for i, c in enumerate(_git("log", "--format=%H", rng).split())}
    if not order:
        return f"changelog freshness: no commits since {tag}"

    # One walk, two answers: `--name-only` with an empty format prints the hash ahead of that commit's paths.
    newest: dict[str, str] = {}
    touches_visible: set[str] = set()
    current = ""
    for line in _git("log", "--format=%H", "--name-only", rng).splitlines():
        if line in order:
            current = line
        elif line.strip() and current:
            key = None
            if line == "CHANGELOG.md":
                key = "changelog"
            elif line.startswith(RELEASE_VISIBLE):
                key = "visible"
                touches_visible.add(current)
            if key and (key not in newest or order[current] < order[newest[key]]):
                newest[key] = current

    visible = newest.get("visible")
    if visible is None:
        return (
            f"changelog freshness: nothing release-visible changed in the "
            f"{len(order)} commit(s) since {tag}"
        )

    changelog = newest.get("changelog")
    if changelog is not None and order[visible] >= order[changelog]:
        return (
            f"changelog freshness: CHANGELOG.md is at or ahead of the newest "
            f"release-visible commit, over {len(order)} commit(s) since {tag}"
        )

    if "CHANGELOG.md" in _git("status", "--porcelain", "--", "CHANGELOG.md"):
        return (
            "changelog freshness: CHANGELOG.md is modified in the working tree "
            "and counts as current"
        )

    # Read from the walk above: `git show` on a merge prints no paths.
    cut = order[changelog] if changelog is not None else len(order)
    waived, owed = [], []
    for h, i in sorted(order.items(), key=lambda kv: kv[1]):
        if i >= cut or h not in touches_visible:
            continue
        subject = _git("log", "-1", "--format=%h %s", h).strip()
        body = _git("log", "-1", "--format=%B", h)
        (waived if NO_CHANGELOG in body else owed).append(subject)

    if owed:
        listing = "\n".join(f"      {s}" for s in owed)
        last = (
            "none"
            if changelog is None
            else _git("log", "-1", "--format=%h %s", changelog).strip()
        )
        fail(
            f"CHANGELOG.md is behind the code: {len(owed)} commit(s) since {tag} "
            f"changed a release-visible path after the last changelog edit "
            f"({last}).\n"
            f"    A release cut here would ship them undocumented, and no other "
            f"check in this file can see it.\n{listing}\n"
            f"    Write the entries, or mark a commit `{NO_CHANGELOG}` if it "
            f"genuinely owes none."
        )
        return "changelog freshness: BEHIND"

    return (
        f"changelog freshness: {len(waived)} release-visible commit(s) since {tag} "
        f"are marked `{NO_CHANGELOG}` and none is unaccounted for"
    )


def main() -> int:
    authority = load_toml("Cargo.toml")["workspace"]["package"]["version"]

    # A check's summary sentence asserts its rule, so it is withheld when the check added a failure (snapshot of `failures` around each call).
    lines: list[str] = []
    for call in (
        check_versions,
        lambda: check_publishable(authority),
        lambda: check_changelog(authority),
        check_recipe_references,
        check_markdown_tables,
        check_relative_links,
        check_front_page_versions,
        check_distribution_name,
        check_decision_status_citations,
        check_changelog_freshness,
        check_line_citations,
    ):
        before = len(failures)
        summary = call()
        if len(failures) == before:
            lines.append(summary)

    if failures:
        # A failing run prints the measurements (`notes`) and the surviving verdicts; a failed check loses its summary.
        for line in [*lines, *notes]:
            print(f"artifact-versions: {line}")
        # stdout is block-buffered under a pipe, so flush before the failure block on stderr.
        sys.stdout.flush()
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

    for line in [*lines, *notes]:
        print(f"artifact-versions: {line}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
