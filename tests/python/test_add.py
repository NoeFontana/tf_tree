"""Smoke tests for the `add` FFI entrypoint and package metadata."""

from rust_python_template import __version__, add


def test_add():
    assert add(2, 3) == 5


def test_add_handles_negatives():
    assert add(-1, 1) == 0
    assert add(-5, -7) == -12


def test_version():
    assert __version__ == "0.1.0"
