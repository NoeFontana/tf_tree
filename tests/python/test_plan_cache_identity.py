"""Issue #196 from the Python side: two ``tf_tree.Tree`` objects on one thread
must not answer for each other.

The defect and its fix are both in the Rust facade — ``PyTree.lookup`` calls
``tf_tree::Tree::lookup`` and keeps no cache of its own, so this binding
inherited the bug and inherits the fix with no change on this side. That is
exactly why the test belongs here rather than only in
``crates/tf_tree/tests/plan_cache_identity.rs``: "the binding delegates" is a
load-bearing assumption asserted in prose, and ``docs/PHASE3.md`` §7.2
contemplates a Python-side cache for GIL-free throughput, which would
reintroduce the defect with nothing on this side to catch it.

**Not a line-for-line copy of ``static_plans_are_not_shared_between_trees``.**
That case turns on a static edge's transform being folded into the plan, and
``tf_tree.build`` cannot declare a static edge (see
``test_edges_are_parent_child_pairs_and_exclude_the_sentinel``). The shape used
instead is the Rust suite's third,
``dynamic_plans_are_not_shared_across_differing_topology``, which is the one
that fails on dynamic edges alone: same frame names in the same order, same
edge count — so the frame ids *and* the topology generation collide — but a
different shape, so the stolen plan walks edges that exist in both arenas and
means something else in each.

Two trees of *identical* topology would pass with the defect present, because a
plan over dynamic edges is a list of edge indices and evaluating it against the
second arena reads the second arena's samples. A Python test built that way
would be the one that reports the bug as absent.
"""

import numpy as np
import tf_tree


def _tx(x):
    """``[qw qx qy qz tx ty tz]`` for a pure-x translation."""
    return [1.0, 0.0, 0.0, 0.0, x, 0.0, 0.0]


def test_two_trees_on_one_thread_do_not_share_a_compiled_plan():
    chain = tf_tree.build([("root", "mid"), ("mid", "tip")])
    tf_tree.push(chain, "mid", "root", 0, _tx(1.0))
    tf_tree.push(chain, "tip", "mid", 0, _tx(10.0))

    star = tf_tree.build([("root", "mid"), ("root", "tip")])
    tf_tree.push(star, "mid", "root", 0, _tx(5.0))
    tf_tree.push(star, "tip", "root", 0, _tx(7.0))

    # The precondition for the collision, not an incidental detail: the names
    # were interned in the same order, so the two arenas agree on every frame
    # id, and both trees declare two edges, so they agree on the generation.
    assert chain.frames() == star.frames() == ["root", "mid", "tip"]

    assert chain.lookup("root", "tip", 0)[0, 3] == 11.0
    # 12.0 — the chain's two-step plan walked over the star's edges — is the
    # wrong answer this test exists to fail on. It belongs to neither tree.
    assert star.lookup("root", "tip", 0)[0, 3] == 7.0
    # Back to the first tree, so a "fix" that merely evicted on every foreign
    # lookup would still have to answer for it.
    assert chain.lookup("root", "tip", 0)[0, 3] == 11.0


def test_a_rebuilt_tree_is_not_the_tree_it_replaced():
    """No two trees alive at once: build, look up, drop, rebuild.

    The allocator hands the same address back, so an identity keyed on the
    arena's base pointer would leave every iteration of a rebuild loop — a test
    suite's fixture, a reopen-on-reconnect loop — answering with the first
    tree's numbers. This is the fourth Rust case, and it is the one a Python
    user is most likely to hit, because rebuilding a tree per notebook cell is
    free in a way it is not from Rust.

    **The shape has to alternate**, and that is not decoration. The Rust case
    rebuilds one static chain with different transforms, which Python cannot
    declare; a rebuild loop of one *dynamic* shape with different samples passes
    with the defect present, because the stolen plan is a list of edge indices
    that reads the new arena's samples and gets the new arena's answer. Measured:
    with `Tree::cache_scope` mutated to a constant, that version of this test
    still passed and the version below failed with 12.0 against 7.0.
    """
    answers = []
    for i in range(4):
        if i % 2 == 0:
            tree = tf_tree.build([("root", "mid"), ("mid", "tip")])
            tf_tree.push(tree, "mid", "root", 0, _tx(1.0))
            tf_tree.push(tree, "tip", "mid", 0, _tx(10.0))
        else:
            tree = tf_tree.build([("root", "mid"), ("root", "tip")])
            tf_tree.push(tree, "mid", "root", 0, _tx(5.0))
            tf_tree.push(tree, "tip", "root", 0, _tx(7.0))
        answers.append(tree.lookup("root", "tip", 0)[0, 3])
        del tree
    np.testing.assert_array_equal(answers, [11.0, 7.0, 11.0, 7.0])
