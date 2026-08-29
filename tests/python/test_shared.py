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


@shm
@pytest.mark.filterwarnings(
    "ignore:This process .* is multi-threaded:DeprecationWarning"
)
def test_a_forked_child_is_refused_by_the_introspection_calls_too(runtime_dir):
    """**An empty list is the wrong way to say "you forked".**

    `Tree.frames`, `Tree.edges` and `Plan.edges` walk the `ArenaView` rather
    than evaluating through a `Guard`, so they do not inherit the refusal the
    test above pins. `Tree::view` substitutes a one-frame, zero-edge poison
    arena for a detached tree — which is right, because it makes reading the
    vanished mapping impossible — and the consequence is that an unguarded walk
    *succeeds*, returning `[]`. A `multiprocessing` worker would read that as a
    corrupt or empty arena and go looking for the wrong bug;
    `docs/PHASE5.md` §4.3 makes `fork` the expected way these users arrive.

    The plan is compiled **before** the fork on purpose: `Tree.plan` refuses in
    the child on its own, so compiling there would test the guard that already
    exists instead of the one this pins.

    Mutant: delete the `if tree.detached()` guard from ``frames_impl``,
    ``edges_impl`` and ``plan_edges_impl`` (`crates/tf_tree_py/src/offline.rs`).
    Applied: all three calls return `[]` in the child, which exits 12 instead of
    0 — the codes are `or`-ed so the *first* unrefused call is the one reported,
    and 13 or 14 alone would name the other two. The exit status is the only
    channel here: an assertion raised inside a fork child is invisible to
    pytest.
    """
    tree = tf_tree.open(mode="rw", create=EDGES)
    with tree.publisher("base", "map") as pub:
        pub.push(1_000, [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0])
    plan = tree.plan("map", "base")
    # The parent answers all three; the child must not.
    assert tree.frames() and tree.edges() and plan.edges()

    pid = os.fork()
    if pid == 0:  # pragma: no cover — the child never returns to pytest
        status = 0
        for code, call in ((12, tree.frames), (13, tree.edges), (14, plan.edges)):
            try:
                call()
                status = status or code  # answered instead of refusing
            except tf_tree.TfTreeError:
                pass
            except Exception:
                status = status or code + 100  # refused, but as the wrong type
        os._exit(status)

    _, wstatus = os.waitpid(pid, 0)
    assert os.WIFEXITED(wstatus), (
        "the child was killed by a signal, not refused: "
        f"signal {os.WTERMSIG(wstatus) if os.WIFSIGNALED(wstatus) else '?'}"
    )
    assert os.WEXITSTATUS(wstatus) == 0


@shm
@pytest.mark.filterwarnings(
    "ignore:This process .* is multi-threaded:DeprecationWarning"
)
def test_a_forked_child_identifies_the_arena_as_gone_not_as_in_process(runtime_dir):
    """**All-zero is a spelling that already means something else.**

    `Tree.instance_uuid` is `self.view().header().instance_uuid`, and
    `Tree::view` substitutes the `alloc_zeroed` poison arena for a detached
    tree — so before this guard the call returned `"0" * 32`, which is exactly
    what `test_an_in_process_tree_has_no_instance_uuid` pins as the *in-process*
    answer. Two peers comparing uuids to chase a split brain would have
    concluded they had never shared an arena at all.

    `__repr__` is the deliberate exception and the second half of this test: a
    repr that raises breaks `print`, the REPL echo and every debugger pane,
    which is where a fork victim is standing. It must not raise, and it must say
    the word rather than print an instance the poison arena invented.

    Exit codes, because an assertion in a fork child is invisible to pytest:
    20 `instance_uuid` answered instead of refusing; 21 it raised the wrong
    type; 22 `repr` raised at all; 23 `repr` did not name the fork; 24 `repr`
    still showed an instance.

    Three mutants, each applied to `crates/tf_tree_py/src/tree.rs`, built and
    observed before being reverted:

    * **A** — delete the `if self.inner.detached()` arm from
      ``PyTree::instance_uuid``. Child exits **20**.
    * **B** — delete ``__repr__``'s `if self.inner.detached()` test and keep
      only the `else` body, so the repr describes the poison arena. Child exits
      **23** (not 24: the poison header is `alloc_zeroed`, so that branch
      suppresses the instance as if this were an in-process tree — which is the
      indistinguishability the guard is for).
    * **C** — make ``__repr__``'s detached arm print both, `" detached-by-fork
      instance={…}"`. Child exits **24**. This is what makes 24 load-bearing;
      without C it is unreachable, given B.

    21 and 22 are not separately mutated: they exist to tell one failure apart
    from another in the one channel a fork child has, not as guards of their own.
    """
    tree = tf_tree.open(mode="rw", create=EDGES)
    parent_uuid = tree.instance_uuid()
    assert parent_uuid != "0" * 32
    assert parent_uuid[:8] in repr(tree)

    pid = os.fork()
    if pid == 0:  # pragma: no cover — the child never returns to pytest
        status = 0
        try:
            tree.instance_uuid()
            status = status or 20
        except tf_tree.TfTreeError:
            pass
        except Exception:
            status = status or 21
        try:
            text = repr(tree)
        except Exception:
            status = status or 22
        else:
            if "detached-by-fork" not in text:
                status = status or 23
            if "instance=" in text:
                status = status or 24
        os._exit(status)

    _, wstatus = os.waitpid(pid, 0)
    assert os.WIFEXITED(wstatus), (
        "the child was killed by a signal, not refused: "
        f"signal {os.WTERMSIG(wstatus) if os.WIFSIGNALED(wstatus) else '?'}"
    )
    assert os.WEXITSTATUS(wstatus) == 0


@shm
def test_a_python_consumer_recovers_an_arena_whose_owner_died(runtime_dir):
    """**Recovery from Python — `docs/decisions/0044`.**

    Until these three methods existed, an all-Python fleet whose arena owner was
    `SIGKILL`ed *could not rejoin it*. The survivors keep their participant
    bytes, so `docs/PHASE2.md` §3.4 step 4 refuses every new create with
    `ArenaHeldButUnreachable`, and the one call that ends that state was Rust
    only — and took `&mut self`, which `PyTree`'s `Arc<Tree>` cannot satisfy.
    The documented recovery was to stop every attached process.

    The owner has to be a **separate process**: only the kernel takes its locks
    away without its cooperation, which is the whole state under test. It is
    started with `subprocess`, not `multiprocessing`, because a fork of this
    (multi-threaded) process is the *other* failure this file tests.
    """
    owner = subprocess.Popen(
        [
            sys.executable,
            "-c",
            "import tf_tree, time;"
            "t = tf_tree.open(mode='rw', create=[('map','base'),('base','cam')]);"
            "print('owning', flush=True);"
            "time.sleep(3600)",
        ],
        stdout=subprocess.PIPE,
        text=True,
        env={**os.environ, "TF_TREE_RUNTIME_DIR": runtime_dir},
    )
    try:
        assert owner.stdout.readline().strip() == "owning", "the owner did not come up"

        tree = tf_tree.open(mode="rw")

        # The owner is alive: the loop is cheap and does nothing.
        assert not tree.owner_lost()
        assert tree.inherit_ownership() == "OwnerAlive"

        # `wait` after `kill`, so the kernel has torn the descriptors down — its
        # ownership byte and its participant byte are released with no
        # cooperation from it.
        owner.kill()
        owner.wait(timeout=30)

        assert tree.owner_lost(), "the owner is gone and its socket hung up"
        assert tree.inherit_ownership() == "Inherited", (
            "the sole read-write survivor should have taken the vacant role"
        )

        # And it settles rather than re-attempting the ownership lock every
        # cycle: this process is the owner now (`docs/decisions/0043`).
        assert not tree.owner_lost(), (
            "an owner that reads its own death would retry the lock forever"
        )

        # The dead owner's own participant record is collected — one of exactly
        # two states the owner's hangup callback structurally cannot reach,
        # because nothing hangs up on an owner. Asserted as a number because the
        # number is the evidence.
        assert tree.reap_dead() == 1, "the dead owner's record should be collected"
        assert tree.reap_dead() == 0, "and a second sweep must find nothing"
    finally:
        if owner.poll() is None:
            owner.kill()
            owner.wait(timeout=30)


@shm
def test_a_read_only_consumer_is_told_it_cannot_inherit(runtime_dir):
    """D18 working, said out loud rather than by silence.

    An owner writes the participant table on every grant and a `PROT_READ`
    mapping cannot, so a fleet of read-only consumers cannot rescue itself — and
    read-only is the consumer *default*. A Python caller needs that to be a
    value it can branch on, not an exception it has to parse.

    **The owner has to be dead for this to be observable**, and finding that out
    is what the first version of this test was for: `inherit_ownership` checks
    "is there anything to inherit" *before* "could I do it if there were", so a
    read-only consumer beside a live owner is told `"OwnerAlive"`. That ordering
    is right — reporting a capability limit when there is nothing to do anyway
    would send an operator looking for a fleet-wide problem that does not exist
    — so the test kills the owner rather than the code changing.
    """
    owner = subprocess.Popen(
        [
            sys.executable,
            "-c",
            "import tf_tree, time;"
            "t = tf_tree.open(mode='rw', create=[('map','base'),('base','cam')]);"
            "print('owning', flush=True);"
            "time.sleep(3600)",
        ],
        stdout=subprocess.PIPE,
        text=True,
        env={**os.environ, "TF_TREE_RUNTIME_DIR": runtime_dir},
    )
    try:
        assert owner.stdout.readline().strip() == "owning"

        ro = tf_tree.open(mode="ro")
        assert ro.inherit_ownership() == "OwnerAlive", (
            "with an owner alive there is nothing to inherit, whatever this "
            "consumer's mapping permits"
        )

        owner.kill()
        owner.wait(timeout=30)

        assert ro.owner_lost(), "the owner is gone"
        assert ro.inherit_ownership() == "ReadOnly", (
            "a read-only mapping cannot write the participant table, so it "
            "cannot be the heir — and it must be told so, not left guessing"
        )
        assert ro.reap_dead() == 0, "a read-only mapping may not write the arena"
        # And it keeps reading straight through, which is the half of D18 that
        # makes the refusal acceptable.
        assert ro.plan("base", "map") is not None
    finally:
        if owner.poll() is None:
            owner.kill()
            owner.wait(timeout=30)
