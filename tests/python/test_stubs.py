"""The stubs must not fall behind the module (`docs/PHASE3.md` §9).

Hand-written stubs are right for this API — generated ones cannot express the
scalar-vs-array overloads on `Plan.at`, which are the most important thing a
user sees. But the failure mode of hand-writing is not being wrong on day one;
it is a method added in Rust that never reaches the stub, and nothing noticing.

So: signatures are ours, **existence is checked**. That is the half that rots.

**Existence is checked against `tf_tree`, not against `tf_tree._core`.** The
extension module is not what anybody imports, and comparing the stub to it left
a hole exactly the shape of the one thing between them: `Publisher` was a class
in `_core` and in the stub, and `tf_tree.Publisher` was an `AttributeError`,
because `__init__.py` re-exports name by name. Every assertion below that can
be phrased against the package is.
"""

import ast
import pathlib

import tf_tree
from tf_tree import _core

STUB = pathlib.Path(tf_tree.__file__).with_name("_core.pyi")

#: Names the package adds on top of `_core`. `open` is `open_arena` under the
#: spelling `docs/PHASE3.md` §4.1 promises, and shadows the builtin inside
#: `__init__` only.
PACKAGE_ONLY = {"open"}


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


def _public(obj: object) -> set[str]:
    return {n for n in dir(obj) if not n.startswith("_")}


def test_every_public_module_symbol_is_declared():
    missing = _public(_core) - _stub_names()
    assert not missing, (
        f"these exist in the module but not in _core.pyi: {sorted(missing)}. "
        "Add them to the stub — a user's type checker cannot see them otherwise."
    )


def test_the_stub_declares_nothing_the_module_lacks():
    """The other direction: a stub entry with no implementation is a lie.

    It type-checks at the call site and raises `AttributeError` at runtime,
    which is worse than having no stub at all.
    """
    phantom = _stub_names() - _public(_core)
    assert not phantom, (
        f"declared in _core.pyi but absent from the module: {sorted(phantom)}"
    )


def test_every_public_core_symbol_is_re_exported_by_the_package():
    """`_core` is private; `tf_tree` is the API. They must not disagree.

    `__init__.py` lists its re-exports by hand — which is what makes
    `from tf_tree import *` and `tf_tree.X` well defined, and what makes a
    forgotten line invisible: `tf_tree.Publisher` raised `AttributeError` for
    exactly as long as nothing compared these two sets. The stub tests above
    cannot catch it, because both sides of *those* are `_core`.

    Mutant: drop `Publisher` from `__init__.py`'s `from ._core import (...)`
    => `AssertionError: public in tf_tree._core but not re-exported by
    tf_tree/__init__.py: ['Publisher']`. Dropping it from `__all__` *instead*
    leaves it reachable as `tf_tree.Publisher` and is the next test's failure,
    not this one's.
    """
    missing = _public(_core) - _public(tf_tree)
    assert not missing, (
        f"public in tf_tree._core but not re-exported by tf_tree/__init__.py: "
        f"{sorted(missing)}. A user importing `tf_tree` cannot reach them."
    )


def test_dunder_all_is_exactly_the_package_namespace():
    """`__all__` is what `import *` binds, and the typing spec's re-export
    marker for a `py.typed` package — which this is.

    Two failures, opposite in shape: a name in `__all__` that does not exist
    (an `AttributeError` the moment anyone runs `from tf_tree import *`), and a
    public name reachable as `tf_tree.X` but absent from `__all__` (which
    `import *` then does not bind, so the package exposes something its own
    declared surface does not admit to).

    Mutant: drop `"Publisher"` from `__all__` while keeping the import
    => `AssertionError: __all__ and the package namespace disagree: only in
    __all__ [], only in the namespace ['Publisher']`. Dropping the import
    instead makes the *first* branch fire, on the same name.
    """
    declared = set(tf_tree.__all__)
    phantom = {n for n in declared if not hasattr(tf_tree, n)}
    assert not phantom, f"in __all__ but not defined: {sorted(phantom)}"
    assert declared == _public(tf_tree), (
        "__all__ and the package namespace disagree: "
        f"only in __all__ {sorted(declared - _public(tf_tree))}, "
        f"only in the namespace {sorted(_public(tf_tree) - declared)}"
    )


def test_the_package_adds_nothing_beyond_the_documented_alias():
    """The one name that is the package's own is `open`, and it is deliberate.

    Anything else here is a name the two tests above do not reach: they check
    existence against `_core.pyi`, which describes the *extension*, so a
    hand-written wrapper living in `__init__.py` could drift from the function
    it wraps with nothing noticing. `open` is exempt because it is an alias and
    the assertion below is that it stayed one.
    """
    extra = _public(tf_tree) - _public(_core) - PACKAGE_ONLY
    assert not extra, f"package-level names with no stub: {sorted(extra)}"
    assert tf_tree.open is tf_tree.open_arena


def test_every_public_method_is_declared():
    """Reached through the package, so a class that is not re-exported fails
    here too rather than being silently skipped."""
    for cls in ("Tree", "Plan", "Publisher"):
        obj = getattr(tf_tree, cls)
        missing = _public(obj) - _stub_members(cls)
        assert not missing, f"{cls}: undeclared methods {sorted(missing)}"
