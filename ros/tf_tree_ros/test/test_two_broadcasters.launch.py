# `docs/PHASE4.md` §9: *"Multi-publisher detection verified against a
# deliberately broken launch file."* This file is that launch file, and the
# test that reads its output.
#
# # What this adds over `test_attribution.cpp`
#
# That gtest constructs two publishers on one edge **in one process** and
# asserts the second is dropped and both nodes are named. Two `rclcpp::Node`s in
# one process typically share a DDS participant, so the interesting half of
# §5.3 — that `rmw_message_info_t::publisher_gid` and
# `TopicEndpointInfo::endpoint_gid()` are the same sixteen bytes *across
# processes* — is not exercised by it at all. Here the bridge and the two
# broadcasters are three separate processes discovering each other over a real
# RMW, which is the configuration an operator actually misconfigures.
#
# # What it asserts, and why not more
#
# `first_writer_wins` (the default) is what a robot runs, so that is what this
# launches. The observable is `bridge_handle.cpp`'s §5.4 sentence —
# *"<A> and <B> have both been publishing odom -> base_link"* — logged at ERROR
# by the bridge process. **Which node is owner is a race** and the fixture does
# not pretend otherwise: whichever broadcaster's sample is ingested first wins,
# so the assertion requires both names to be present and does not fix their
# order. An assertion that named the owner would pass or fail on process
# start-up ordering.
#
# # Two discovery windows, not one, and only one of them closes on its own
#
# The §5.4 log fires on `first_time` **per edge** — once. A GID not yet resolved
# to a node name at that instant is rendered `<unattributed>`
# (`crates/tf_tree_bridge/src/lib.rs:285`; note `docs/PHASE4.md` §5.4 and several
# source comments spell this `<unknown publisher>`, which appears as a literal
# nowhere in the Rust), and the line is never re-emitted.
#
# The **intruder**'s window closes on its own: its name only has to be resolved
# at the instant the line fires, so waiting before its first sample is enough.
# The **owner**'s does not. `authority.rs:246` clones the publisher into the
# owner slot on the first accepted sample and never updates it, so a first
# sample that lands before the graph lists that node freezes an unattributed
# owner for the life of the bridge — and the line fires later, naming it.
#
# So both broadcasters wait before publishing, for different reasons. This is a
# property of the diagnostic worth knowing about rather than a trick to make a
# test pass: an operator whose two nodes start simultaneously can get
# `<unattributed>` in the one line they are given.

import os
import unittest

import launch
import launch_ros.actions
import launch_testing
import launch_testing.actions
import launch_testing.asserts
import pytest

# One dynamic edge, which both broadcasters will claim. `capacity` matches
# `test_attribution.cpp`'s so the two fixtures describe the same tree.
TOPOLOGY = """
[[edge]]
parent = "odom"
child = "base_link"
kind = "dynamic"
capacity = 256
"""

PARENT = "odom"
CHILD = "base_link"
# Named for §5.4's own sentence: "your `/ekf` and `/odom_node` have both been
# publishing `odom -> base_link` for eight months".
FIRST = "ekf"
SECOND = "odom_node"


@pytest.mark.launch_test
def generate_test_description():
    bridge = launch_ros.actions.Node(
        package="tf_tree_ros",
        executable="tf_tree_bridge",
        name="tf_tree_bridge",
        output="screen",
        parameters=[{"topology_config": TOPOLOGY}],
    )
    # **The owner needs a discovery delay too, and for a different reason than
    # the intruder.** The intruder's name has to be resolved at the instant the
    # one-shot §5.4 line fires. The owner's is captured earlier and *frozen*: the
    # authority table clones the publisher's name when it accepts the first
    # sample on the edge, so a first sample that lands before the graph lists
    # this node freezes an unattributed owner for the life of the bridge, and the
    # line fires later naming it. An earlier revision delayed only the second
    # broadcaster and left the first publishing at t=0 — mitigating the window
    # that closes on its own and not the one that does not.
    first = launch_ros.actions.Node(
        package="tf_tree_ros",
        executable="conflicting_broadcaster",
        name=FIRST,
        output="screen",
        parameters=[
            {
                "parent": PARENT,
                "child": CHILD,
                "rate_hz": 50.0,
                "start_delay_s": 1.0,
            }
        ],
    )
    # The deliberate break: a second broadcaster on the same edge. The delay is
    # for discovery, not for sequencing the authority decision — see the header.
    second = launch_ros.actions.Node(
        package="tf_tree_ros",
        executable="conflicting_broadcaster",
        name=SECOND,
        output="screen",
        parameters=[
            {
                "parent": PARENT,
                "child": CHILD,
                "rate_hz": 50.0,
                "start_delay_s": 3.0,
            }
        ],
    )
    return (
        launch.LaunchDescription(
            [bridge, first, second, launch_testing.actions.ReadyToTest()]
        ),
        {"bridge": bridge, "first": first, "second": second},
    )


class TestTwoBroadcastersOnOneEdge(unittest.TestCase):
    def test_the_bridge_names_both_publishers(self, proc_output, bridge):
        """§5.4's sentence reaches an operator, with both node names in it."""
        sentence = f"have both been publishing {PARENT} -> {CHILD}"
        # Generous: three processes have to discover each other, the second
        # broadcaster waits 3 s before its first sample, and a loaded CI runner
        # is slower than a desktop at all three.
        proc_output.assertWaitFor(sentence, process=bridge, timeout=60)

        # `proc_output[bridge]` yields `launch.events.process.ProcessIO`, whose
        # payload is `.text` **as bytes**. `str(event)` is a repr and does not
        # contain the output at all, so the obvious spelling finds nothing and
        # raises `StopIteration` *after* `assertWaitFor` has already passed —
        # which reads as "the sentence never arrived" when it did. Measured, not
        # guessed: the first run of this file failed exactly that way while the
        # bridge's log carried the line.
        line = next(
            text
            for text in (
                t.text.decode(errors="replace") for t in proc_output[bridge]
            )
            if sentence in text
        )
        # Both names, in either order: which one owns the edge is decided by
        # whichever sample the bridge ingested first.
        # **The window this fixture is built to stay out of — and the string
        # here is the one the code actually emits.** An earlier revision asserted
        # `<unknown publisher>`, which is the spelling `docs/PHASE4.md` §5.4 and
        # several source comments use and which appears as a **literal nowhere in
        # the Rust**: `grep -rn '<unknown publisher>' crates/ --include=*.rs`
        # matches only comments. `crates/tf_tree_bridge/src/lib.rs:285` is the
        # emitted value, and it is `<unattributed>`. So that assertion could
        # never have fired — an anti-vacuity check that was itself vacuous, which
        # is this repository's most-repeated defect and was introduced here in
        # the same commit that mutated the test to prove it could fail.
        # **Two placeholders, because the code has two.** A publisher with no
        # GID at all is `<unattributed>`; a publisher whose GID the graph has not
        # resolved to a name renders as its **full GID key**, `<gid:...>` —
        # `lib.rs:368-374`, whose own comment says it is deliberately "not
        # `<unknown publisher>`" so that two unnamed publishers stay
        # distinguishable in a diagnostic about two publishers. In this fixture
        # the RMW supplies GIDs, so the unresolved case is the second one.
        self.assertNotIn("<unattributed>", line, line)
        self.assertNotIn("<gid:", line, line)
        # Positive form, and it is the assertion that carries the weight: the
        # sentence has exactly two publisher slots, so if a placeholder took one
        # of them a name is missing. A negative-only check is satisfied by any
        # placeholder a future change might add.
        for name in (FIRST, SECOND):
            self.assertIn(f"/{name}", line, line)


@launch_testing.post_shutdown_test()
class TestCleanShutdown(unittest.TestCase):
    def test_the_bridge_did_not_crash(self, proc_info, bridge):
        """A conflict is a diagnostic, not a fault: `first_writer_wins` keeps
        serving the owner's transforms. A bridge that exited here would mean the
        default policy halts on a misconfiguration it is supposed to survive."""
        launch_testing.asserts.assertExitCodes(
            proc_info, allowable_exit_codes=[0, -2, -15], process=bridge
        )
