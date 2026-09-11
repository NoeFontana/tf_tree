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
# # The `<unknown publisher>` window is real and is designed around
#
# The §5.4 log fires on `first_time` **per edge** — once. A GID that has not yet
# been resolved to a node name at that instant is logged as
# `<unknown publisher>`, and the log line is never re-emitted, though the
# internal record is rewritten when the names resolve. So a fixture whose second
# broadcaster starts publishing before it is discoverable tests discovery
# latency and fails intermittently with a message about attribution. The second
# broadcaster therefore carries `start_delay_s`: it creates its publisher, waits
# for discovery, and only then publishes. That is a property of the diagnostic
# worth knowing about rather than a trick to make a test pass — an operator
# whose two nodes start simultaneously can get `<unknown publisher>` in the one
# line they are given.

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
    first = launch_ros.actions.Node(
        package="tf_tree_ros",
        executable="conflicting_broadcaster",
        name=FIRST,
        output="screen",
        parameters=[{"parent": PARENT, "child": CHILD, "rate_hz": 50.0}],
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
        self.assertIn(f"/{FIRST}", line, line)
        self.assertIn(f"/{SECOND}", line, line)
        # The window this fixture is built to stay out of. If this fires, the
        # diagnostic reached the operator without a name in it, which is the
        # failure mode `start_delay_s` exists to avoid.
        self.assertNotIn("<unknown publisher>", line, line)


@launch_testing.post_shutdown_test()
class TestCleanShutdown(unittest.TestCase):
    def test_the_bridge_did_not_crash(self, proc_info, bridge):
        """A conflict is a diagnostic, not a fault: `first_writer_wins` keeps
        serving the owner's transforms. A bridge that exited here would mean the
        default policy halts on a misconfiguration it is supposed to survive."""
        launch_testing.asserts.assertExitCodes(
            proc_info, allowable_exit_codes=[0, -2, -15], process=bridge
        )
