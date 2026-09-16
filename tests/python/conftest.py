"""What the suite shares, in the file pytest imports before any test module.

`_stub_annotations` had two spellings and five copies: a module-level
`from test_stubs import _stub_annotations` in `test_domains.py`, and four more
inside function bodies in `test_errors.py` and `test_shared.py`. All five
resolve only because pytest's `prepend` import mode puts this directory on
`sys.path` before it imports a test module — a real mechanism, and not one to
lean on, because what they reach for *is a test module*. A test module is not a
library; `conftest.py` is, pytest imports it first, and `from conftest import
...` is now the one spelling.

`test_stubs.py` keeps every assertion about the stub. What moved here is only
the reading of it, which four files need and one of them owned.
"""

import ast
import pathlib

import tf_tree

#: The stub as shipped, beside the extension it describes. Read from
#: `tf_tree.__file__` rather than from this file's checkout path, so what is
#: parsed is what was installed into the interpreter under test.
STUB = pathlib.Path(tf_tree.__file__).with_name("_core.pyi")

#: A child name past the 48 bytes a frame record stores (`docs/decisions/0058`
#: measurement 5), so the arena's stored pair and the typed pair differ.
#:
#: One copy, in one place, because the two rows that use it — `test_errors.py`'s
#: stored-vs-typed push and `test_shared.py`'s refused claim — are deleted by
#: the same change, the one that lands `0027` and makes `intern` refuse a name
#: over 48 bytes. Two copies of the constant is two chances to delete one row
#: and leave the other asserting a difference that can no longer exist.
LONG_CHILD = "sensor_" + "x" * 60


def _stub_class(cls: str) -> ast.ClassDef | None:
    tree = ast.parse(STUB.read_text())
    for node in tree.body:
        if isinstance(node, ast.ClassDef) and node.name == cls:
            return node
    return None


def _stub_annotations(cls: str) -> dict[str, str]:
    """``{attribute: annotation source}`` for one class body in the stub."""
    node = _stub_class(cls)
    if node is None:
        return {}
    return {
        n.target.id: ast.unparse(n.annotation)
        for n in node.body
        if isinstance(n, ast.AnnAssign) and isinstance(n.target, ast.Name)
    }
