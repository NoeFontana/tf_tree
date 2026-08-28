"""The time domain a query carries (`docs/decisions/0038`).

`tf_tree.Domain` is an **open trait** whose tag is a `const`, so a Rust caller
states the domain in the type and a mistake is a compile error. Python cannot
name the type, and until `0038` it did not carry the tag either: every query
site in the binding constructed a `SystemDomain` stamp, so on an arena whose
edges are not tag `0` — which is exactly what `ros/tf_tree_ros` tells an
operator to configure for a simulated tree — every Python lookup failed with
`TimeDomainMismatch`, permanently and by construction, with no argument the
caller could pass to say otherwise.

**The reproduction `0038` asks for is at the foot of this file, and getting it
there took a fixture.** *"Open a tag-1 arena, plan in domain 1, read a
transform"* cannot be written against an arena Python *builds*: an edge's domain
is set at builder time (`EdgeCfg::domain`), and neither `tf_tree.build` nor
`tf_tree.open(create=...)` exposes it, so nothing this package ships can bring a
non-zero-domain arena into existence.

That mattered more than it looks. **Measured:** reverting all six `Plan`-handle
query sites to the pre-`0038` `SystemDomain` spelling, rebuilding, and running
this whole suite gave *158 passed* — every one of them green. Only
`Tree.lookup`'s site was caught, because its check is per call and fires on the
tag-0 arena a Python test can build; a plan handle's tag can only ever be `0`
when the plan is dynamic, since the plan-time check refuses every other value,
and it is never compared when the plan is not.

What closes it is `testdata/frozen/sensor_domain.tft`, a committed frozen arena
whose dynamic edges carry tag 1. `docs/PHASE5.md` §2.1 is NORMATIVE that a frozen
`.tft` is read by the identical `Plan::at` code as a live arena, and
`tf_tree.open_file` already reads one — so the reproduction needs **no new API on
any surface**, which is why it is a fixture and not a publishing keyword.
`crates/tf_tree/examples/gen_domain_fixture.rs` regenerates it and
`crates/tf_tree/tests/frozen.rs` stops it going stale.
"""

import pathlib

import numpy as np
import pytest
import tf_tree

#: The committed tag-1 arena, the only non-zero-domain tree this package can
#: reach. Resolved from this file and not from `tf_tree.__file__`: the suite is
#: only ever runnable from the checkout, so the path is always there and no skip
#: — which would be a silent vacuous pass — is needed.
FIXTURE = (
    pathlib.Path(__file__).parents[2] / "testdata" / "frozen" / "sensor_domain.tft"
)


#: `plan(target, source)`; the samples live on `map -> base`.
EDGES = [("map", "base"), ("base", "cam")]


@pytest.fixture
def tree():
    """A tag-`0` chain — the only kind Python can build (see the module docs)."""
    t = tf_tree.build(EDGES)
    tf_tree.push(t, "base", "map", 1_000, [1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0])
    tf_tree.push(t, "base", "map", 2_000, [1.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0])
    return t


def test_the_four_builtin_tags_are_exported_as_plain_ints():
    """`0038` §3: names rather than magic numbers, and `int` rather than an enum.

    The values are the trait's, not a second copy of them — they are compiled in
    from `Domain::TAG`, so a tag that moved in the core would move here. The
    assertion on the *numbers* is what pins them as an on-the-wire fact: they
    are written into every arena's edge records, so changing one is a format
    change and not a rename.

    `int` and not `IntEnum`: tags from `4` up belong to whoever declares a
    domain, so a closed set standing in for an open one would either make a
    driver's own tag unrepresentable or hand it back as a bare `int` that
    compares equal to nothing in the enum.
    """
    assert (tf_tree.SYSTEM_DOMAIN, tf_tree.SENSOR_DOMAIN) == (0, 1)
    assert (tf_tree.SIM_DOMAIN, tf_tree.STEADY_DOMAIN) == (2, 3)
    for name in ("SYSTEM_DOMAIN", "SENSOR_DOMAIN", "SIM_DOMAIN", "STEADY_DOMAIN"):
        tag = getattr(tf_tree, name)
        assert type(tag) is int, f"{name} is {type(tag)}, not a plain int"


def test_a_plan_in_the_wrong_domain_is_refused_at_plan_time(tree):
    """The check `0038` moved, in the place it moved to.

    Refused at `plan()` rather than at every `at()` in the hot loop, for the
    reason the record gives: a domain is a property of a *route* through the
    tree, not of an instant, so it cannot legitimately differ between two
    queries on one plan — and this is the one moment both frame names are still
    strings, so the message can say which route disagreed instead of only which
    two integers did.

    Before `0038` this call was a `TypeError`: there was no keyword.
    """
    with pytest.raises(tf_tree.TfTreeError) as e:
        tree.plan("map", "base", domain=tf_tree.SIM_DOMAIN)
    msg = str(e.value)
    assert '"base" -> "map"' in msg, msg
    assert "domain 0" in msg and "domain 2" in msg, msg
    # The remedy, which is the whole difference between a wall and a mistake.
    assert "domain=0" in msg, msg


def test_the_default_is_zero_and_not_the_paths_own_domain(tree):
    """`0038`'s *Rationale*: defaulting to the path's own domain is the tempting
    one-line fix and is wrong.

    It would make every existing caller silently correct **and every mistaken
    caller silently correct too**, deleting the check for exactly the population
    D9 exists to protect. So the default is a stated `0` — right for a
    wall-clock arena, and refused for any other, as the test above shows.
    """
    default = tree.plan("map", "base")
    stated = tree.plan("map", "base", domain=tf_tree.SYSTEM_DOMAIN)
    assert np.array_equal(default.at(1_500), stated.at(1_500))


def test_lookup_takes_the_domain_the_same_way(tree):
    """The convenience tier had the same defect and gets the same keyword.

    It is a per-call check here rather than a plan-time one — the plan is cached
    rather than returned, so there is no handle to hang it on — which is why the
    sentence names two tags rather than a route.
    """
    assert np.array_equal(
        tree.lookup("map", "base", 1_500),
        tree.lookup("map", "base", 1_500, domain=tf_tree.SYSTEM_DOMAIN),
    )
    with pytest.raises(tf_tree.TfTreeError) as e:
        tree.lookup("map", "base", 1_500, domain=tf_tree.SENSOR_DOMAIN)
    msg = str(e.value)
    assert "time domain 0" in msg and "domain 1" in msg, msg
    # `0038` §3 again: the prose gains the remedy. Before the keyword existed
    # this sentence described a wall.
    assert "tree.plan(" in msg, msg


def test_a_path_with_nothing_to_sample_accepts_any_tag(tree):
    """`0038` §4: the check moves, it does not *widen*.

    `Plan::check_domain_tag` fires only when the plan has a dynamic step, and a
    plan-time check that dropped that condition would be a **new** refusal
    smuggled in beside an earlier one: a path with nothing to sample consults no
    clock, and `Plan::domain()` answers `0` for one whether or not `0` means
    anything there.

    A self-plan is the only such path Python can build — `tf_tree.build`
    declares every edge dynamic — and it folds to zero steps.

    Mutant (applied, run): drop `samples_anything(&plan) &&` from
    `PyTree::plan` => `TfTreeError: the path "map" -> "map" is sampled in time
    domain 0, and this plan was asked for domain 2`. It takes the smoke test
    below with it, which plans the same empty path in `SENSOR_DOMAIN`.
    """
    p = tree.plan("map", "map", domain=tf_tree.SIM_DOMAIN)
    assert p.depth() == 0
    assert np.array_equal(p.at(1_500), np.eye(4))


def test_every_query_shape_reaches_the_engine_with_the_handles_tag(tree):
    """All five query shapes carry the handle's tag, on one plan.

    Eight sites in `crates/tf_tree_py/src/tree.rs` constructed a `SystemDomain`
    stamp or instantiated that type parameter, and `0038` §1 gives the core a
    tagged sibling for each of the five *shapes* they cover. This walks every
    one of them with a non-zero tag in the handle, which is what an argument
    swapped between `domain` and `layout` — the two `u8`-ish parameters the
    batch entry points now take next to each other — would fail on.

    **It is a smoke test and says so.** The tag is not *observable* here: the
    path folds to zero steps, so the engine's comparison is skipped, and only a
    non-zero-domain arena would make the difference between passing the tag and
    hard-coding `0` visible. Python cannot build one (see the module docstring).
    """
    p = tree.plan("map", "map", domain=tf_tree.SENSOR_DOMAIN)
    eye = np.eye(4)
    stamps = np.array([1_200, 1_500, 1_800], dtype=np.int64)

    assert np.array_equal(p.at(1_500), eye)  # at_tagged, scalar
    assert np.array_equal(p.at(stamps), np.broadcast_to(eye, (3, 4, 4)))  # batch

    out = np.zeros((4, 4))
    p.at_into(1_500, out)  # at_tagged, into a caller's buffer
    assert np.array_equal(out, eye)

    batch = np.zeros((3, 4, 4))
    p.at_into(stamps, batch)  # at_many_into_tagged
    assert np.array_equal(batch, np.broadcast_to(eye, (3, 4, 4)))

    # The f64 and f32 layout paths — `at_many_into_tagged` and its `_f32` twin,
    # which take `domain` and `layout` adjacently.
    assert p.at(1_500, layout="quat").tolist() == [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
    assert p.at(stamps, layout="affine32").dtype == np.float32

    knots, poses = p.adaptive(1_200, 1_800)  # at_adaptive_tagged
    assert len(knots) == len(poses) >= 2
    assert np.array_equal(poses[0], eye)


def test_the_stub_and_the_package_agree_on_the_four_names():
    """The names are module-level ints, which is the one shape
    `tests/python/test_stubs.py` did not look at until `0038` added them.

    That file's `_stub_names` now collects `ast.AnnAssign` as well, so this is
    belt-and-braces rather than a second gate; what it adds is the *package*
    half — `__init__.py` re-exports by hand, so a name can exist in `_core`,
    exist in the stub, and still be an `AttributeError` on `tf_tree`.
    """
    from tf_tree import _core

    for name in ("SYSTEM_DOMAIN", "SENSOR_DOMAIN", "SIM_DOMAIN", "STEADY_DOMAIN"):
        assert getattr(tf_tree, name) == getattr(_core, name)
        assert name in tf_tree.__all__


def test_a_tag_outside_a_byte_is_refused_by_the_conversion(tree):
    """A domain tag is a `u8` in the arena, and the refusal is PyO3's own.

    Not a hand-written `ValueError` naming the range: `capacity`, `stamp_ns` and
    every other integer this binding takes is converted the same way, and a
    bespoke message on this one parameter would be the second spelling of a
    refusal the crate already has (`docs/PROJECT.md` §6). What the test pins is
    that the *bound* is a byte — the width edge records store, so it is a format
    fact rather than a parameter choice — and that a mistyped tag cannot arrive
    truncated.
    """
    for bad in (256, -1):
        with pytest.raises(OverflowError):
            tree.plan("map", "base", domain=bad)
    with pytest.raises(TypeError):
        tree.plan("map", "base", domain="sim")


@pytest.fixture
def tag1():
    """The committed tag-1 frozen arena: `map -> odom -> base_link -> lidar`.

    Two dynamic edges at `SENSOR_DOMAIN` and one static edge, 16 samples per
    dynamic edge at 10 ms, so stamps 0..150 ms are answerable. Regenerated by
    `cargo run -p tf_tree --features shm --example gen_domain_fixture`.
    """
    assert FIXTURE.is_file(), (
        f"{FIXTURE} is missing; regenerate it with `cargo run -p tf_tree "
        f"--features shm --example gen_domain_fixture`"
    )
    return tf_tree.open_file(str(FIXTURE))


def test_a_tag_one_arena_answers_a_tag_one_query(tag1):
    """`0038` step 4's reproduction, which had no fixture to run against.

    This is the *acceptance* direction, and it is the one that was missing.
    Every other behavioural test in this file exercises a refusal, which a
    tag-0 arena can produce; only a non-zero-domain arena can distinguish
    "carried the handle's tag" from "hard-coded 0", because
    `check_domain_tag` compares 0 against 0 in every other case.
    """
    assert tag1.plan("odom", "map", domain=tf_tree.SENSOR_DOMAIN).at(75_000_000) is not None

    # And the refusal, on the same arena: tag 0 is now the wrong answer, which
    # is the state `ros/tf_tree_ros` warns an operator they are creating.
    with pytest.raises(Exception) as e:
        tag1.plan("odom", "map", domain=tf_tree.SYSTEM_DOMAIN)
    assert "domain" in str(e.value).lower()


def test_every_query_shape_carries_the_tag_on_a_tag_one_arena(tag1):
    """All five shapes, against an arena that can tell the difference.

    The sibling of `test_every_query_shape_reaches_the_engine_with_the_handles_tag`
    above, which walks the same five shapes on a *zero-step* path and says in
    its own docstring that the tag is not observable there. Here the path folds
    two dynamic edges and a static one, so every shape's `domain` argument
    reaches `check_domain_tag` and is compared against 1 — and reverting any one
    of the six binding sites to `Stamp::<SystemDomain>` makes that shape raise.

    This is what replaced the structural source-reading check: it is the
    behavioural claim that check could only stand in for.
    """
    p = tag1.plan("lidar", "map", domain=tf_tree.SENSOR_DOMAIN)
    stamps = np.array([50_000_000, 75_000_000, 100_000_000], dtype=np.int64)

    scalar = p.at(75_000_000)                      # at_tagged
    assert scalar.shape == (4, 4)
    assert np.isfinite(scalar).all()

    batch = p.at(stamps)                           # at_many_into_tagged
    assert batch.shape == (3, 4, 4)
    assert np.isfinite(batch).all()

    out = np.zeros((4, 4))
    p.at_into(75_000_000, out)                     # at_tagged, caller buffer
    assert np.array_equal(out, scalar)

    into = np.zeros((3, 4, 4))
    p.at_into(stamps, into)                        # at_many_into_tagged
    assert np.array_equal(into, batch)

    quat = p.at(75_000_000, layout="quat")         # the f64 layout path
    assert quat.shape == (7,)

    aff = p.at(stamps, layout="affine32")          # at_many_into_f32_tagged
    assert aff.dtype == np.float32

    knots, poses = p.adaptive(50_000_000, 100_000_000)   # at_adaptive_tagged
    assert len(knots) == len(poses) >= 2

    # The convenience tier takes the same tag, per call rather than per plan.
    assert tag1.lookup("odom", "map", 75_000_000, domain=tf_tree.SENSOR_DOMAIN) is not None


def test_a_composed_route_is_not_the_identity(tag1):
    """The control that stops every assertion above passing on an empty fold.

    `test_every_query_shape_reaches_the_engine_with_the_handles_tag` runs on
    `map -> map`, where every answer is the identity and a shape that silently
    returned one would look correct. Here the route crosses two dynamic edges
    and a static one, so the identity is the *wrong* answer and a fold that did
    nothing is visible.
    """
    p = tag1.plan("lidar", "map", domain=tf_tree.SENSOR_DOMAIN)
    a = p.at(50_000_000)
    b = p.at(100_000_000)
    assert not np.allclose(a, np.eye(4)), "the composed route returned the identity"
    assert not np.allclose(a, b), "the route did not move between two stamps"
