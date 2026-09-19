# `docs/PHASE4.md` §9: *"Multi-publisher detection verified against a deliberately
# broken launch file."* This file is that launch file, and the test that reads its
# output. # What this adds over `test_attribution.cpp` That gtest constructs two
# publishers on one edge **in one process** and asserts the second is dropped and both
# nodes are named.

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
    # **The owner needs a discovery delay too, and for a different reason than the
    # intruder.** The intruder's name has to be resolved at the instant the one-shot
    # §5.4 line fires.
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
        # Generous: three processes have to discover each other, the second broadcaster
        # waits 3 s before its first sample, and a loaded CI runner is slower than a
        # desktop at all three.
        proc_output.assertWaitFor(sentence, process=bridge, timeout=60)

        # `proc_output[bridge]` yields `launch.events.process.ProcessIO`, whose payload
        # is `.text` **as bytes**.
        line = next(
            text
            for text in (
                t.text.decode(errors="replace") for t in proc_output[bridge]
            )
            if sentence in text
        )
        # Both names, in either order: which one owns the edge is decided by whichever
        # sample the bridge ingested first. **The window this fixture is built to stay
        # out of — and the string here is the one the code actually emits.** An earlier
        # revision asserted `<unknown publisher>`, which is the spelling
        # `docs/PHASE4.md` §5.4 and several source comments use and which appears as a
        # **literal nowhere in the Rust**: `grep -rn '<unknown publisher>' crates/
        # --include=*.rs` matches only comments.
        self.assertNotIn("<unattributed>", line, line)
        self.assertNotIn("<gid:", line, line)
        # Positive form, and it is the assertion that carries the weight: the sentence
        # has exactly two publisher slots, so if a placeholder took one of them a name
        # is missing.
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
