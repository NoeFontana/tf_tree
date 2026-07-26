"""The stubs must not fall behind the module (`docs/PHASE3.md` §9).

Hand-written stubs are right for this API — generated ones cannot express the
scalar-vs-array overloads on `Plan.at`, which are the most important thing a
user sees. But the failure mode of hand-writing is not being wrong on day one;
it is a method added in Rust that never reaches the stub, and nothing noticing.

So: signatures are ours, **existence is checked**. That is the half that rots.
"""

import ast
import pathlib

import tf_tree
from tf_tree import _core

STUB = pathlib.Path(tf_tree.__file__).with_name("_core.pyi")


def _stub_names() -> set[str]:
    tree = ast.parse(STUB.read_text())
    names = set()
    for node in tree.body:
        if isinstance(node, (ast.ClassDef, ast.FunctionDef)):
            names.add(node.name)
    return names


def _stub_members(cls: str) -> set[str]:
    tree = ast.parse(STUB.read_text())
    for node in tree.body:
        if isinstance(node, ast.ClassDef) and node.name == cls:
            return {n.name for n in node.body if isinstance(n, ast.FunctionDef)}
    return set()


def test_every_public_module_symbol_is_declared():
    actual = {n for n in dir(_core) if not n.startswith("_")}
    missing = actual - _stub_names()
    assert not missing, (
        f"these exist in the module but not in _core.pyi: {sorted(missing)}. "
        "Add them to the stub — a user's type checker cannot see them otherwise."
    )


def test_the_stub_declares_nothing_the_module_lacks():
    """The other direction: a stub entry with no implementation is a lie.

    It type-checks at the call site and raises `AttributeError` at runtime,
    which is worse than having no stub at all.
    """
    actual = {n for n in dir(_core) if not n.startswith("_")}
    phantom = _stub_names() - actual
    assert not phantom, (
        f"declared in _core.pyi but absent from the module: {sorted(phantom)}"
    )


def test_every_public_method_is_declared():
    for cls in ("Tree", "Plan"):
        obj = getattr(_core, cls)
        actual = {n for n in dir(obj) if not n.startswith("_")}
        missing = actual - _stub_members(cls)
        assert not missing, f"{cls}: undeclared methods {sorted(missing)}"
