"""Shared-memory behaviour from Python (`docs/PHASE2.md`, `docs/PHASE3.md` §8.1)."""

import gc
import os
import pathlib
import subprocess
import sys
import tempfile

import numpy as np
import pytest
import tf_tree
from conftest import LONG_CHILD, _stub_annotations

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
    # Same *segment*, not merely the same name.
    assert a.instance_uuid() == b.instance_uuid()
    assert a.instance_uuid() != "0" * 32
    assert a.is_shared() and b.is_shared()
    assert a.is_writable() and not b.is_writable()


@shm
def test_a_released_claim_can_be_retaken_from_another_process(runtime_dir):
    """**The claim lease must actually be released.**"""
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
    # Expected, and the reason the test exists: the arena's owner thread makes this
    # process multi-threaded, and forking a multi-threaded process is exactly what
    # `multiprocessing` does on Linux.
    "ignore:This process .* is multi-threaded:DeprecationWarning"
)
def test_a_forked_child_is_refused_rather_than_faulting(runtime_dir):
    """**`multiprocessing` defaults to `fork` on Linux**, so this is how users meet it."""
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
        # `_exit`, not `exit`: the interpreter's teardown would run the inherited
        # objects' finalizers, and what those do in a fork child is the *other* half of
        # this, covered on the Rust side by `crates/tf_tree_bench/tests/fork.rs`.
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
def test_a_forked_child_is_refused_with_child_process_detached_error(
    runtime_dir, tmp_path
):
    """**`docs/PHASE3.md` §8.1 is NORMATIVE and names the class.**"""
    tree = tf_tree.open(mode="rw", create=EDGES)
    pub = tree.publisher("base", "map")
    pub.push(1_000, [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0])
    plan = tree.plan("map", "base")
    pose = [1.0, 0.0, 0.0, 0.0, 4.0, 5.0, 6.0]
    calls = {
        "push": lambda: pub.push(2_000, pose),
        "push_many": lambda: pub.push_many(
            np.array([2_000], dtype=np.int64), np.array([pose])
        ),
        "module push": lambda: tf_tree.push(tree, "cam", "base", 2_000, pose),
        "lookup": lambda: tree.lookup("map", "base", 1_000),
        "plan": lambda: tree.plan("map", "base"),
        "plan.at": lambda: plan.at(1_000),
        "frames": tree.frames,
        "push_many empty": lambda: pub.push_many(
            np.zeros(0, dtype=np.int64), np.zeros((0, 7))
        ),
        "freeze": lambda: tree.freeze(tmp_path / "child.tft"),
    }

    read_end, write_end = os.pipe()
    pid = os.fork()
    if pid == 0:  # pragma: no cover — the child never returns to pytest
        os.close(read_end)
        for name, call in calls.items():
            os.write(write_end, f"{name}=".encode())
            try:
                call()
                outcome = "answered"
            except Exception as e:
                outcome = type(e).__name__
            os.write(write_end, f"{outcome}\n".encode())
        os._exit(0)

    os.close(write_end)
    with os.fdopen(read_end, "rb") as r:
        report = r.read().decode()
    _, wstatus = os.waitpid(pid, 0)
    assert os.WIFEXITED(wstatus) and os.WEXITSTATUS(wstatus) == 0, (wstatus, report)
    got = dict(line.split("=", 1) for line in report.splitlines())
    assert got == dict.fromkeys(calls, "ChildProcessDetachedError"), got

    # A subclass, so every `except tf_tree.TfTreeError` written before it
    # existed still catches it.
    assert issubclass(tf_tree.ChildProcessDetachedError, tf_tree.TfTreeError)
    pub.release()


@shm
@pytest.mark.filterwarnings(
    "ignore:This process .* is multi-threaded:DeprecationWarning"
)
def test_a_forked_child_is_refused_by_the_introspection_calls_too(runtime_dir):
    """**An empty list is the wrong way to say "you forked".**"""
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
    """**All-zero is a spelling that already means something else.**"""
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
def test_a_refused_claim_raises_edge_already_claimed_with_the_holders_slot(
    runtime_dir,
):
    """`EdgeAlreadyClaimedError.owner_slot` and `.edge` (`0058` step 5)."""
    edges = [("map", "base"), ("base", LONG_CHILD)]
    creator = subprocess.Popen(
        [
            sys.executable,
            "-c",
            "import tf_tree, time;"
            f"t = tf_tree.open(mode='rw', create={edges!r});"
            "print('owning', flush=True);"
            "time.sleep(3600)",
        ],
        stdout=subprocess.PIPE,
        text=True,
        env={**os.environ, "TF_TREE_RUNTIME_DIR": runtime_dir},
    )
    try:
        assert creator.stdout.readline().strip() == "owning", "no creator"
        holder = tf_tree.open(mode="rw")
        other = tf_tree.open(mode="rw")
        with (
            holder.publisher("base", "map"),
            holder.publisher(LONG_CHILD, "base"),
        ):
            with pytest.raises(tf_tree.EdgeAlreadyClaimedError) as short:
                other.publisher("base", "map")
            with pytest.raises(tf_tree.EdgeAlreadyClaimedError) as long:
                tf_tree.push(other, LONG_CHILD, "base", 1_000, [1.0, 0, 0, 0, 0, 0, 0])

        for e in (short.value, long.value):
            assert type(e) is tf_tree.EdgeAlreadyClaimedError
            assert e.owner_slot == 1, vars(e)
            assert set(vars(e)) == set(_stub_annotations("EdgeAlreadyClaimedError"))
        assert short.value.edge == ("map", "base")
        assert long.value.edge == holder.edges()[1]
        assert long.value.edge != ("base", LONG_CHILD)
    finally:
        creator.kill()
        creator.wait(timeout=30)


@shm
def test_opening_a_name_nothing_serves_raises_arena_absent(runtime_dir):
    """`ArenaAbsentError` (`0058` step 7), the retry a supervisor writes."""
    with pytest.raises(tf_tree.ArenaAbsentError) as excinfo:
        tf_tree.open(name="tf_tree_test_nothing_serves_this")
    e = excinfo.value
    assert type(e) is tf_tree.ArenaAbsentError

    assert set(vars(e)) == set(_stub_annotations("ArenaAbsentError")) == set()


def _rendezvous_child() -> pathlib.Path:
    """The Rust test helper `tf_tree_rendezvous_child`, which the pytest recipes build."""
    root = pathlib.Path(__file__).resolve().parents[2]
    target = pathlib.Path(os.environ.get("CARGO_TARGET_DIR") or root / "target")
    exe = target / "debug" / "tf_tree_rendezvous_child"
    assert exe.is_file(), (
        f"{exe} is missing; `just py-test` builds it, or run `cargo build -p "
        "tf_tree --features shm --bin tf_tree_rendezvous_child`"
    )
    return exe


@shm
def test_a_peer_reparent_raises_topology_changed_with_both_generations(runtime_dir):
    """`TopologyChangedError.plan_generation` and `.current_generation` (`0058`)."""
    tree = tf_tree.open(mode="rw", create=EDGES)
    plan = tree.plan("map", "cam")
    child = subprocess.Popen(
        [str(_rendezvous_child()), "join-reparent"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env={**os.environ, "TF_TREE_RUNTIME_DIR": runtime_dir},
    )
    try:
        assert child.stdout.readline().strip() == "joined", "the helper did not join"
        child.stdin.write("go\n")
        child.stdin.flush()
        line = child.stdout.readline().strip()
        assert line == "reparented", f"the helper did not re-parent: {line!r}"

        with pytest.raises(tf_tree.TopologyChangedError) as excinfo:
            plan.at(1_000)
        e = excinfo.value
        assert type(e) is tf_tree.TopologyChangedError
        assert e.plan_generation < e.current_generation, vars(e)

        assert set(vars(e)) == set(_stub_annotations("TopologyChangedError"))
    finally:
        child.kill()
        child.wait(timeout=30)


@shm
def test_a_python_consumer_recovers_an_arena_whose_owner_died(runtime_dir):
    """**Recovery from Python — `docs/decisions/0044`.**"""
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
        # A read-only attach takes a lock-file participant byte too.
        second = tf_tree.open(mode="ro")

        # The owner is alive: the loop is cheap and does nothing.
        assert not tree.owner_lost()
        assert tree.inherit_ownership() == "OwnerAlive"

        # `wait` after `kill`, so the kernel has torn the descriptors down — its
        # ownership byte and its participant byte are released with no cooperation from
        # it.
        owner.kill()
        owner.wait(timeout=30)

        assert tree.owner_lost(), "the owner is gone and its socket hung up"

        with pytest.raises(tf_tree.ArenaHeldButUnreachableError) as excinfo:
            tf_tree.open(mode="rw")
        held = excinfo.value
        assert type(held) is tf_tree.ArenaHeldButUnreachableError
        assert held.ownership_held is False
        assert len(held.holder_slots) >= 2, held.holder_slots
        assert held.holder_slots == tuple(sorted(held.holder_slots))
        assert not hasattr(held, "first_pid")

        annotated = set(_stub_annotations("ArenaHeldButUnreachableError"))
        assert set(vars(held)) == annotated
        # **Not a precondition, and that is measured rather than assumed**: with these
        # two lines removed the inheritance below still answers `Inherited`. A read-only
        # participant never holds byte 0, so it cannot contend for the vacant role —
        # only its *participant* byte is held, and that is what `holder_slots` above is
        # for.
        del second
        gc.collect()
        assert tree.inherit_ownership() == "Inherited", (
            "the sole read-write survivor should have taken the vacant role"
        )

        # And it settles rather than re-attempting the ownership lock every
        # cycle: this process is the owner now (`docs/decisions/0043`).
        assert not tree.owner_lost(), (
            "an owner that reads its own death would retry the lock forever"
        )

        # The dead owner's own participant record is collected — one of exactly two
        # states the owner's hangup callback structurally cannot reach, because nothing
        # hangs up on an owner.
        assert tree.reap_dead() == 1, "the dead owner's record should be collected"
        assert tree.reap_dead() == 0, "and a second sweep must find nothing"
    finally:
        if owner.poll() is None:
            owner.kill()
            owner.wait(timeout=30)


@shm
def test_a_read_only_consumer_is_told_it_cannot_inherit(runtime_dir):
    """D18 working, said out loud rather than by silence."""
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
