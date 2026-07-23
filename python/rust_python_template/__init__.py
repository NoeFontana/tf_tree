"""rust-python-template: a Rust-core + PyO3 + Python-wrapper library template."""

from importlib.metadata import version as _version

from rust_python_template._core import add

__version__ = _version("rust-python-template")
__all__ = ["__version__", "add"]
