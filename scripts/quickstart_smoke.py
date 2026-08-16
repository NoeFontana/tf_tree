#!/usr/bin/env python3
"""Execute the README's first-five-minutes snippet and check what it prints.

Run it with the interpreter the extension is installed into:

    .venv/bin/python scripts/quickstart_smoke.py

which is what `just quickstart` does as its last step.

**The snippet is not copied into this file, it is read out of `README.md` and
executed.** That is the whole point. A copy here would be a second spelling of
the quickstart (`docs/PROJECT.md` §6) and — worse — it would keep passing while
the README drifted away from it, which is the exact defect this release found
three times over: the status table against the `§0.0` tables, the CLI's
"arrives in Phase 2" help, and a documented `build + install` step that only
built. A reader who follows the README runs *the README*, so that is what gets
run here.

The expected output is read out of the snippet too, from the `# -> ` marker on
its `print` line. There is therefore no number in this file to disagree with
the README about: the README states the claim, and this script decides whether
the engine still honours it.

What this does **not** do is check that the snippet is good documentation, or
that `## First five minutes` is still the section a newcomer should read. It
checks that the code in it runs and prints what it says it prints.
"""

from __future__ import annotations

import contextlib
import io
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
README = ROOT / "README.md"

# The section a newcomer is pointed at. Matched by prefix so the rest of the
# heading ("…, with no data at all") can be rewritten without breaking the gate
# — the same latitude `just msrv` gives the sentence that states the MSRV.
HEADING = "## First five minutes"

PY_FENCE_RE = re.compile(r"^```python\n(.*?)^```", re.S | re.M)
EXPECTATION_RE = re.compile(r"#\s*->\s*(.+?)\s*$", re.M)


def die(message: str) -> None:
    print(f"quickstart-smoke: {message}", file=sys.stderr)
    raise SystemExit(1)


def extract() -> tuple[str, str]:
    """Return (snippet, expected stdout), or exit with what went wrong.

    Every "expected exactly one" below is a real constraint rather than
    defensive noise: with two python blocks in the section there is no fact of
    the matter about which one the reader runs, and with two `# ->` markers
    there is none about what the output should be. Failing with the count is
    more useful than guessing and passing.
    """
    text = README.read_text()

    starts = [m.start() for m in re.finditer(rf"^{re.escape(HEADING)}", text, re.M)]
    if len(starts) != 1:
        die(
            f"expected exactly one `{HEADING}` heading in README.md, found "
            f"{len(starts)}. If that section was renamed, rename HEADING here "
            f"in the same commit."
        )
    section = text[starts[0] :]
    nxt = re.search(r"^## ", section[len(HEADING) :], re.M)
    if nxt:
        section = section[: len(HEADING) + nxt.start()]

    blocks = PY_FENCE_RE.findall(section)
    if len(blocks) != 1:
        die(
            f"expected exactly one ```python block under `{HEADING}`, found "
            f"{len(blocks)}. The quickstart is one thing a reader pastes."
        )
    snippet = blocks[0]

    expectations = EXPECTATION_RE.findall(snippet)
    if len(expectations) != 1:
        die(
            f"expected exactly one `# -> ` marker in that block, found "
            f"{len(expectations)}. The marker is what this script compares against, "
            f"so it is the README's claim about its own output and there has to be "
            f"exactly one of it."
        )
    return snippet, expectations[0]


def main() -> int:
    snippet, expected = extract()

    captured = io.StringIO()
    try:
        with contextlib.redirect_stdout(captured):
            # A fresh namespace with __name__ set, so a snippet that ever grows
            # an `if __name__ == "__main__"` guard still runs its body.
            exec(compile(snippet, str(README), "exec"), {"__name__": "__main__"})
    except ModuleNotFoundError as exc:
        die(
            f"{exc}. Run this with the interpreter the extension is installed into:\n"
            f"    .venv/bin/python scripts/quickstart_smoke.py\n"
            f"or just run `just quickstart`, which installs it first."
        )
    except Exception as exc:  # noqa: BLE001 — the snippet's own failure is the finding
        die(
            f"the README's quickstart snippet raised {type(exc).__name__}: {exc}\n"
            f"    The documented first five minutes do not work. The snippet is in "
            f"README.md under `{HEADING}`."
        )

    got = captured.getvalue().strip()
    if got != expected:
        die(
            f"the README's quickstart snippet printed {got!r}, and README.md says it "
            f"prints {expected!r}.\n"
            f"    One of the two is wrong. The snippet and the `# -> ` marker are "
            f"both in README.md under `{HEADING}`."
        )

    print(f"quickstart-smoke: README.md's `{HEADING}` snippet runs and prints {got}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
