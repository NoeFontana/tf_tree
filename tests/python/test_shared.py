"""Shared-memory behaviour from Python (`docs/PHASE2.md`, `docs/PHASE3.md` §8.1).

These are the tests that were missing when `PyPublisher` held a `Publisher`
instead of an `EdgeWriter`. That was a `transmute` between two *different*
types, which compiled only while their sizes happened to agree, and it dropped
the two fields that are not in `Publisher`: the claim lease and the fork
generation. Both failures were silent, and nothing here or in Rust could see
either — `crates/tf_tree_py` is excluded from the workspace, so `just test`
never built it at all.
"""

import gc
import os
import pathlib
import subprocess
import sys
import tempfile

import numpy as np
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
def test_a_forked_child_is_refused_with_child_process_detached_error(
    runtime_dir, tmp_path
):
    """**`docs/PHASE3.md` §8.1 is NORMATIVE and names the class.**

    Every refusal in a fork child raised the base `TfTreeError`, on a judgement
    that a detached tree is "not a condition a program branches on". The
    program that branches on it is a retry loop: `SlotContended`,
    `InternContended` and `LeaseContended` also reach Python as `TfTreeError`
    saying "retry", so a loop catching `TfTreeError` could not stop on a handle
    that will never work again except by matching message text, which
    `docs/API.md` R5 says is not a promise.

    The test above catches `Exception`, so it pins *refused, not faulted* and is
    blind to the class. This one pins the class on every entry point that has
    its own route to the refusal: the publisher's `push` and `push_many` (both
    through `push_err`'s class, not `detached_err`), the module-level `push`
    (through `resolve_frame`), a precompiled plan's `at` and `lookup` (through
    `lookup_err`), `plan` itself, the introspection walk, `freeze`, and a
    `push_many` of nothing.

    **`freeze` faulted** — `SIGSEGV` in the child, status 139, and from before
    this class existed: `Tree::freeze_to` reads the manifest and the arena's
    bytes directly, and neither it nor `offline::freeze_impl` asked
    `detached()`. **An empty `push_many` answered `None`**, because the fork
    check lives in the per-sample `push` and zero samples never reach it.

    **The report travels through a pipe**, not the exit status: an assertion in
    a fork child is invisible to pytest, and a pipe lets the parent say *which*
    call raised *what* instead of decoding a number.

    Mutants, each applied, rebuilt and run, and the outcome observed:

    * `detached_err` raising `TfTreeError` again => this test fails on
      `lookup`, `module push`, `plan`, `plan.at` and `frames` reporting
      `TfTreeError`; `push` and `push_many` still pass, because they do not go
      through it.
    * `push_class`'s `ChildDetached` arm answering `TfTreeError::new_err` =>
      it fails on `push` and `push_many` alone.
    * `push_many`'s wrapper in `crates/tf_tree_py/src/tree.rs` building
      `TfTreeError::new_err` again instead of taking `push_class` => it fails
      on `push_many` alone — the sentence is prefixed there, and the class used
      to be re-chosen with it.
    * `freeze_impl`'s `if tree.detached()` guard deleted => the child dies with
      status 139 and the report ends at `freeze=`, every call before it having
      answered `ChildProcessDetachedError`; the rest of `tests/python` passes.
    * `push_many`'s `st.is_empty()` guard deleted => it fails on
      `{'push_many empty': 'answered'}` alone.

    **Each line is written as its call finishes**, and its name before the call
    starts, so a child that dies mid-loop still says which call killed it.
    """
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


#: A child name past the 48 bytes a frame record stores, so the arena's pair and
#: the typed pair differ (`docs/decisions/0058` measurement 5).
LONG_CHILD = "sensor_" + "x" * 60


@shm
def test_a_refused_claim_raises_edge_already_claimed_with_the_holders_slot(
    runtime_dir,
):
    """`EdgeAlreadyClaimedError.owner_slot` and `.edge` (`0058` step 5).

    A subprocess creates the arena and claims nothing, so it holds slot 0
    (`CREATOR_SLOT`); this process joins read-write as the first joiner, slot
    1, and claims two edges; a second read-write handle, a different participant,
    is refused both. **`owner_slot == 1` is what holds the slot**: a claim held
    by the creator would read `0`, which a hard-coded `0` could not be told
    apart from.

    The second edge's child is 67 bytes, so `.edge` — the stored pair, a member
    of `Tree.edges()` — differs from the pair the caller typed there and nowhere
    else. **That half depends on `0027`**: if `intern` comes to refuse names
    over 48 bytes it cannot be built, and the change that lands `0027` deletes
    it.

    `owner_slot`'s `None` arm, the `CLAIMING` sentinel, is not reached: no test
    can hold a claim word in that window.

    Mutants, each applied alone, rebuilt and run with ``just py-test``; each
    fails this test and nothing else:

    * ``owner_slot`` set to ``Some(0)`` => ``{'edge': ('map', 'base'),
      'owner_slot': 0}``, ``assert 0 == 1``.
    * `claimed_by` answering ``None`` for every slot => ``assert None == 1``.
    * the ``EdgeAlreadyClaimed`` arm guarded ``if false``, so the cause reaches
      the bug-report arm => ``tf_tree.TfTreeError: edge "map" -> "base": tf_tree
      reported a claim failure this binding has no message for ...`` escapes
      ``pytest.raises``.
    * ``.edge`` set from the typed ``(parent, child)`` => the long-name half
      fails, ``At index 1 diff``: the 67-byte typed child against the 48-byte
      stored one. The short edge passes under it, as it must.

    **Not a mutant: `claimed_by` hard-coded to ``Some``**, which would hand a
    handler ``4294967295``. No test can put a claim word in ``CLAIMING``, so
    nothing could kill it; the arm is held by its type and its review.
    """
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

        from test_stubs import _stub_annotations

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
    """`ArenaAbsentError` (`0058` step 7), the retry a supervisor writes.

    `tf_tree.open` without `create=` is `CreatePolicy::Never`, and with no
    participant byte held the rendezvous refuses at once rather than waiting out
    a timeout that could not change the answer — `test_api.py`'s
    `test_open_validates_interp_even_with_nothing_to_create` names this as the
    call past its `interp` check. The class carries nothing, and the stub
    annotates nothing: the Rust variant is a unit.

    Mutant: `open_err`'s ``IpcError::ArenaAbsent`` arm deleted, so the error
    reaches the forwarding arm => this test alone fails, ``tf_tree.TfTreeError:
    no arena is serving and CreatePolicy::Never forbids creating one`` escaping
    ``pytest.raises``.
    """
    with pytest.raises(tf_tree.ArenaAbsentError) as excinfo:
        tf_tree.open(name="tf_tree_test_nothing_serves_this")
    e = excinfo.value
    assert type(e) is tf_tree.ArenaAbsentError

    from test_stubs import _stub_annotations

    assert set(vars(e)) == set(_stub_annotations("ArenaAbsentError")) == set()


def _rendezvous_child() -> pathlib.Path:
    """The Rust test helper `tf_tree_rendezvous_child`, which the pytest recipes build.

    Under the cargo target directory — `$CARGO_TARGET_DIR`, else the workspace's
    `target/` — in `debug/`. **Missing is a failure, not a skip**: a skip would
    let a recipe that stopped building the binary stay green while the test it
    exists for ran nowhere.
    """
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
    """`TopologyChangedError.plan_generation` and `.current_generation` (`0058`).

    The one error a correct program attached to a shared arena routinely meets,
    and no single-process call raises it: none of the Python, CLI or C surfaces
    can re-parent. The Rust helper can — `join-reparent` joins read-write and,
    on a line of stdin, moves `cam` from `base` to `map` — so this process
    serves an arena with the helper's own `layout()` pairs, compiles a plan,
    lets the helper re-parent, and asks the plan again.

    `Plan::at_tagged` checks the generation before the domain or any data, so
    no sample is needed. The strict `<` is what holds the two attributes apart:
    swapped they read greater, and both taken from one field they read equal.

    Mutants, each applied alone, rebuilt and run with ``just py-test``:

    * swap the two ``setattr``s => fails on ``{'plan_generation': 3,
      'current_generation': 2}``, ``assert 3 < 2``.
    * set ``current_generation`` from the variant's ``plan`` too => fails on
      ``{'plan_generation': 2, 'current_generation': 2}``, ``assert 2 < 2``.

    Nothing else in `tests/python` moves under either: no other test raises this
    class.
    """
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

        from test_stubs import _stub_annotations

        assert set(vars(e)) == set(_stub_annotations("TopologyChangedError"))
    finally:
        child.kill()
        child.wait(timeout=30)


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

    **The window between the owner's death and `inherit_ownership` is also
    `ArenaHeldButUnreachableError`'s trigger** (`docs/decisions/0058` step 6):
    the survivors hold their bytes and nothing serves, so a fresh
    `tf_tree.open(mode="rw")` refuses after its 5 s open timeout, which Python
    cannot shorten. A second, read-only participant is attached first so that
    two slots are held — with one, `holder_slots` would be ascending and
    descending at once — and released before the inheritance assertion.

    Mutants, each applied alone, rebuilt and run with ``just py-test``; each
    fails this test and nothing else:

    * ``ownership_held`` set ``true`` => ``assert True is False``.
    * ``holder_slots`` decoded from bit 63 down => ``assert (2, 1) == (1, 2)``.
    * `open_err`'s ``ArenaHeldButUnreachable`` arm deleted, so the forwarding
      arm raises the base class => ``tf_tree.TfTreeError: an arena is alive but
      unreachable: participant slots 0x6 ...`` escapes ``pytest.raises``.
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
        # A read-only attach takes a lock-file participant byte too.
        second = tf_tree.open(mode="ro")

        # The owner is alive: the loop is cheap and does nothing.
        assert not tree.owner_lost()
        assert tree.inherit_ownership() == "OwnerAlive"

        # `wait` after `kill`, so the kernel has torn the descriptors down — its
        # ownership byte and its participant byte are released with no
        # cooperation from it.
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

        from test_stubs import _stub_annotations

        annotated = set(_stub_annotations("ArenaHeldButUnreachableError"))
        assert set(vars(held)) == annotated
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
