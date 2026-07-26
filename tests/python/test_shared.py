"""Shared-memory behaviour from Python (`docs/PHASE2.md`, `docs/PHASE3.md` §8.1).

These are the tests that were missing when `PyPublisher` held a `Publisher`
instead of an `EdgeWriter`. That was a `transmute` between two *different*
types, which compiled only while their sizes happened to agree, and it dropped
the two fields that are not in `Publisher`: the claim lease and the fork
generation. Both failures were silent, and nothing here or in Rust could see
either — `crates/tf_tree_py` is excluded from the workspace, so `just test`
never built it at all.
"""

import os
import subprocess
import sys
import tempfile

import pytest
import tf_tree

#: The topology these tests create. `0004` sizes an arena from its declared
#: edges, so creating one means saying what is in it.
EDGES = [("map", "base"), ("base", "cam")]

shm = pytest.mark.skipif(
    not tf_tree.has_shared_memory(),
    reason="this build cannot share a tree between processes",
)


@pytest.fixture
def runtime_dir(monkeypatch):
    """A scratch rendezvous directory, so tests cannot collide with a real robot."""
    with tempfile.TemporaryDirectory(prefix="tf_tree_py_") as d:
        monkeypatch.setenv("TF_TREE_RUNTIME_DIR", d)
        yield d


@shm
def test_open_creates_and_a_second_open_joins(runtime_dir):
    a = tf_tree.open(mode="rw", create=EDGES)
    b = tf_tree.open(mode="ro")
    # Same *segment*, not merely the same name. Two processes that resolved one
    # name can still hold different arenas if the owner was replaced between
    # their calls, and comparing names cannot tell.
    assert a.instance_uuid() == b.instance_uuid()
    assert a.instance_uuid() != "0" * 32
    assert a.is_shared() and b.is_shared()
    assert a.is_writable() and not b.is_writable()


@shm
def test_a_released_claim_can_be_retaken_from_another_process(runtime_dir):
    """**The claim lease must actually be released.**

    A leaked lease is invisible from inside the process that leaked it — OFD
    locks are self-blind, so the leaker's own `SETLK` succeeds either way. Only
    a *separate process* can see the byte, which is why this shells out.

    With the old `transmute`, `ClaimLease::drop` never ran, so every Python
    publisher leaked its edge's byte for the life of the process. Nothing broke
    immediately: it breaks when a reaper looks at that edge and sees a lease
    held by a process that no longer wants it.
    """
    tree = tf_tree.open(mode="rw", create=EDGES)
    with tree.publisher("base", "map") as pub:
        pub.push(1_000, [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0])
    # The claim is released here. A peer must now be able to take the edge.
    code = (
        "import os, tf_tree;"
        "t = tf_tree.open(mode='rw');"
        "p = t.publisher('base', 'map');"
        "p.push(2_000, [1.0, 0.0, 0.0, 0.0, 9.0, 9.0, 9.0]);"
        "p.release();"
        "print('claimed')"
    )
    out = subprocess.run(
        [sys.executable, "-c", code],
        capture_output=True,
        text=True,
        env={**os.environ, "TF_TREE_RUNTIME_DIR": runtime_dir},
        timeout=30,
    )
    assert out.returncode == 0, f"peer could not claim the released edge:\n{out.stderr}"
    assert "claimed" in out.stdout


@shm
@pytest.mark.filterwarnings(
    # Expected, and the reason the test exists: the arena's owner thread makes
    # this process multi-threaded, and forking a multi-threaded process is
    # exactly what `multiprocessing` does on Linux.
    "ignore:This process .* is multi-threaded:DeprecationWarning"
)
def test_a_forked_child_is_refused_rather_than_faulting(runtime_dir):
    """**`multiprocessing` defaults to `fork` on Linux**, so this is how users
    meet it.

    The arena is mapped `MADV_DONTFORK`: the child has no mapping where it was,
    and every handle it inherited points into a hole in its address space. The
    guard turns that into `ChildDetached`; without it, the child dies of
    `SIGSEGV` inside a `push` that looks perfectly ordinary.

    `WIFEXITED` is the load-bearing assertion. The old code bypassed the fork
    guard entirely — `EdgeWriter::push` checks the generation and
    `Publisher::push` does not — and a test that compared only an exit status
    would have seen a signalled child and had no status to compare.
    """
    tree = tf_tree.open(mode="rw", create=EDGES)
    pub = tree.publisher("base", "map")
    pub.push(1_000, [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0])

    pid = os.fork()
    if pid == 0:
        status = 0
        try:
            pub.push(2_000, [1.0, 0.0, 0.0, 0.0, 4.0, 5.0, 6.0])
            status = 10  # the push should not have succeeded
        except Exception:
            pass
        try:
            tree.lookup("map", "base", 1_000)
            status = status or 11
        except Exception:
            pass
        # `_exit`, not `exit`: the interpreter's teardown would run the
        # inherited objects' finalizers, and what those do in a fork child is
        # the *other* half of this, covered on the Rust side by
        # `crates/tf_tree_bench/tests/fork.rs`.
        os._exit(status)

    _, wstatus = os.waitpid(pid, 0)
    assert os.WIFEXITED(wstatus), (
        "the child was killed by a signal, not refused: "
        f"signal {os.WTERMSIG(wstatus) if os.WIFSIGNALED(wstatus) else '?'}"
    )
    assert os.WEXITSTATUS(wstatus) == 0

    # And the parent is unharmed — it still owns the edge it claimed.
    pub.push(3_000, [1.0, 0.0, 0.0, 0.0, 7.0, 8.0, 9.0])
    pub.release()
