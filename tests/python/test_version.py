"""The three values that attribute a number to a build.

A benchmark result or a bug report is worth very little if nobody can say which
build produced it, and until these landed `tf_tree.__version__` was an
`AttributeError`. That is worse than version skew: skew is a disagreement you
can see, and a missing version is a report you cannot place at all.

**These tests pin existence and type, never a literal.** A test asserting
`__version__ == "0.0.1"` would fail on the release that bumps it, which trains
people to edit the test rather than read it. The one equality here is between
two *files* that both claim to hold the version, which is the assertion that
actually catches something.
"""

import importlib.metadata

import pytest
import tf_tree
from tf_tree import _core


def test_version_is_a_non_empty_string():
    assert isinstance(tf_tree.__version__, str)
    assert tf_tree.__version__


def test_the_package_and_the_extension_report_the_same_version():
    """`__init__.py` re-exports; it must not paraphrase.

    Mutant, run: delete the `from ._core import (__version__ as __version__,)`
    statement in `python/tf_tree/__init__.py`, rebuild
    => three of these fail, this one with `AttributeError: module 'tf_tree' has
    no attribute '__version__'` — which is the exact defect that was shipped.
    """
    assert tf_tree.__version__ == _core.__version__


def test_version_matches_the_installed_distribution():
    """The compiled-in version and the wheel's metadata come from two files.

    `__version__` is `env!("CARGO_PKG_VERSION")` from
    `crates/tf_tree_py/Cargo.toml`; the distribution version is
    `pyproject.toml`'s `[project] version`, which is what maturin stamps on the
    wheel and what `pip` shows. Nothing but this assertion makes a release bump
    both.

    Skipped rather than failed when the metadata is absent: the module imports
    fine from a `cargo test` build directory with nothing installed, and a test
    that fails there would be reporting on the environment, not on the code.

    Mutant, run: replace `env!("CARGO_PKG_VERSION")` in `tf_tree_py`'s `_core`
    with the literal `"0.2.0"`, rebuild
    => `AssertionError: the extension was compiled at 0.2.0 and the installed
    distribution says 0.0.1`. That literal is not a strawman — it is the
    version this crate carried before the 0.0.1 release, so it is precisely
    what a hand-copied string would have left behind.
    """
    try:
        dist = importlib.metadata.version("transform_tree")
    except importlib.metadata.PackageNotFoundError:
        pytest.skip("tf_tree is not installed as a distribution; nothing to compare")
    assert tf_tree.__version__ == dist, (
        f"the extension was compiled at {tf_tree.__version__} and the installed "
        f"distribution says {dist}. Either a release bumped "
        "crates/tf_tree_py/Cargo.toml and pyproject.toml separately, or this is "
        "a stale wheel — reinstall before believing the mismatch. A third cause "
        "if the version is a pre-release: cargo spells it `0.1.0-rc.1` and PEP "
        "440 metadata normalises that to `0.1.0rc1`, and the two files then "
        "disagree on spelling while agreeing on meaning."
    )


def test_arena_format_version_is_a_positive_int():
    v = tf_tree.arena_format_version()
    assert isinstance(v, int)
    assert v > 0


def test_arena_layout_hash_is_an_int_in_u32():
    """It is a `u32` in the arena header, and Python ints are unbounded.

    So the bound is not decoration: a value outside it would mean the
    conversion at the boundary is wrong — a sign-extended negative is what a
    hash with the top bit set looks like if it ever crosses as an `i32`, and it
    would still compare equal to itself and never to anybody else's.
    """
    h = tf_tree.arena_layout_hash()
    assert isinstance(h, int)
    assert 0 <= h <= 0xFFFF_FFFF


def test_the_identity_is_stable_within_a_process():
    """Both read compile-time constants, so a second call cannot differ.

    Cheap to assert and it pins the shape: if either ever grew a per-arena
    answer, it would belong on `Tree` and not on the module.
    """
    assert tf_tree.arena_format_version() == tf_tree.arena_format_version()
    assert tf_tree.arena_layout_hash() == tf_tree.arena_layout_hash()
